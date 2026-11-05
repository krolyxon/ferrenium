//! Commit history: newest-first walks, paged with a cursor, optionally
//! filtered by path and message.
//!
//! Path filters follow git's default history simplification: at a merge
//! that kept one parent's version of the path, only that parent is
//! followed, so side branches whose changes didn't survive are left out
//! (`git log -- path`, not `--full-history`).

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use anyhow::Context;
use gix::bstr::ByteSlice;
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;

use super::tree::{self, EntryKind};
use super::{CommitInfo, ObjectId, commit_info};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LogQuery {
    /// Only commits that change this path (empty: all commits).
    pub path: Vec<u8>,
    /// Case-insensitive match on the message, author name and email.
    pub grep: Option<String>,
    /// Start after this commit (the last one on the previous page).
    pub after: Option<ObjectId>,
    pub page: usize,
    /// Commits a filtered walk may examine before giving up on this page.
    pub scan_limit: usize,
}

impl LogQuery {
    fn filtered(&self) -> bool {
        !self.path.is_empty() || self.grep.is_some()
    }
}

/// Where the "newer" link of a page goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newer {
    /// This is the first page.
    None,
    /// Back to the first page (filtered logs can't step back exactly).
    First,
    After(ObjectId),
}

#[derive(Debug, Clone)]
pub struct LogPage {
    pub commits: Vec<CommitInfo>,
    /// Cursor for the next (older) page.
    pub older: Option<ObjectId>,
    pub newer: Newer,
    /// The filter gave up after `scan_limit` commits; `older` continues.
    pub limit_reached: bool,
}

impl LogPage {
    pub fn weight(&self) -> usize {
        64 + self.commits.iter().map(CommitInfo::weight).sum::<usize>()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("the cursor commit is not in this history")]
    UnknownCursor,
    #[error(transparent)]
    Git(#[from] anyhow::Error),
}

/// One page of the history of `tip`, newest commit first.
pub fn page(repo: &gix::Repository, tip: ObjectId, query: &LogQuery) -> Result<LogPage, LogError> {
    let filtered = query.filtered();
    let grep = query.grep.as_ref().map(|q| q.to_lowercase());
    let mut walk = if query.path.is_empty() {
        Walk::All(
            repo.rev_walk([tip])
                .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
                .all()
                .context("cannot walk history")?,
        )
    } else {
        Walk::Path(PathWalk::new(repo, tip, &query.path)?)
    };

    // Unfiltered: the commits up to and including the cursor, enough to
    // find where the previous page started.
    let mut recent: VecDeque<ObjectId> = VecDeque::new();
    let mut seeking = query.after;
    let mut out = LogPage {
        commits: Vec::new(),
        older: None,
        newer: Newer::None,
        limit_reached: false,
    };
    let mut scanned = 0;
    let mut last_examined = None;
    while let Some(step) = walk.next() {
        let (id, touches) = step?;
        if let Some(cursor) = seeking {
            if !filtered {
                recent.push_back(id);
                if recent.len() > query.page + 1 {
                    recent.pop_front();
                }
            }
            if id == cursor {
                seeking = None;
            }
            continue;
        }
        if filtered {
            if scanned == query.scan_limit {
                out.limit_reached = true;
                out.older = last_examined;
                break;
            }
            scanned += 1;
            last_examined = Some(id);
        }
        if !touches {
            continue;
        }
        let commit = commit_info(repo, id)?;
        if let Some(grep) = &grep
            && !message_matches(repo, &commit, grep)?
        {
            continue;
        }
        if out.commits.len() == query.page {
            out.older = out.commits.last().map(|c| c.id);
            break;
        }
        out.commits.push(commit);
    }
    if seeking.is_some() {
        return Err(LogError::UnknownCursor);
    }
    if query.after.is_some() {
        out.newer = if filtered || recent.len() <= query.page {
            Newer::First
        } else {
            Newer::After(recent[0])
        };
    }
    Ok(out)
}

fn message_matches(repo: &gix::Repository, info: &CommitInfo, grep: &str) -> anyhow::Result<bool> {
    if info.author_name.to_lowercase().contains(grep)
        || info.author_email.to_lowercase().contains(grep)
    {
        return Ok(true);
    }
    let commit = repo.find_commit(info.id).context("commit not found")?;
    let message = commit.message_raw_sloppy().to_str_lossy().to_lowercase();
    Ok(message.contains(grep))
}

enum Walk<'repo> {
    All(gix::revision::Walk<'repo>),
    Path(PathWalk<'repo>),
}

impl Walk<'_> {
    /// The next commit, and whether it passes the path filter.
    fn next(&mut self) -> Option<anyhow::Result<(ObjectId, bool)>> {
        match self {
            Walk::All(walk) => Some(
                walk.next()?
                    .map(|info| (info.id, true))
                    .context("cannot walk history"),
            ),
            Walk::Path(walk) => walk.next().transpose(),
        }
    }
}

/// Newest-first walk that simplifies history for `path`.
struct PathWalk<'repo> {
    repo: &'repo gix::Repository,
    path: Vec<u8>,
    queue: BinaryHeap<(i64, ObjectId)>,
    seen: HashSet<ObjectId>,
    oids: PathOids,
}

impl<'repo> PathWalk<'repo> {
    fn new(repo: &'repo gix::Repository, tip: ObjectId, path: &[u8]) -> anyhow::Result<Self> {
        let mut walk = PathWalk {
            repo,
            path: path.to_vec(),
            queue: BinaryHeap::new(),
            seen: HashSet::new(),
            oids: PathOids::default(),
        };
        walk.push(tip)?;
        Ok(walk)
    }

    fn push(&mut self, id: ObjectId) -> anyhow::Result<()> {
        if !self.seen.insert(id) {
            return Ok(());
        }
        // Missing parents (shallow clones) end the walk there.
        let Ok(commit) = self.repo.find_commit(id) else {
            return Ok(());
        };
        let time = commit.time().context("malformed commit")?.seconds;
        self.queue.push((time, id));
        Ok(())
    }

    fn next(&mut self) -> anyhow::Result<Option<(ObjectId, bool)>> {
        let Some((_, id)) = self.queue.pop() else {
            return Ok(None);
        };
        let commit = self.repo.find_commit(id).context("commit not found")?;
        let parents: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
        let here = self.oids.get(self.repo, id, &self.path)?;
        let mut same = None;
        for parent in &parents {
            if self.oids.get(self.repo, *parent, &self.path)? == here {
                same = Some(*parent);
                break;
            }
        }
        let (touches, follow) = match same {
            // Identical to a parent: not interesting, and only that
            // parent's history explains the path's state.
            Some(parent) => (false, vec![parent]),
            None if parents.is_empty() => (here.is_some(), Vec::new()),
            None => (true, parents),
        };
        for parent in follow {
            self.push(parent)?;
        }
        Ok(Some((id, touches)))
    }
}

/// What one path holds (id and mode), memoized per commit: each commit is
/// usually asked about again as the parent of the next one.
#[derive(Default)]
struct PathOids {
    by_commit: HashMap<ObjectId, Option<(ObjectId, EntryKind)>>,
}

impl PathOids {
    fn get(
        &mut self,
        repo: &gix::Repository,
        commit: ObjectId,
        path: &[u8],
    ) -> anyhow::Result<Option<(ObjectId, EntryKind)>> {
        if let Some(found) = self.by_commit.get(&commit) {
            return Ok(*found);
        }
        let found = tree::entry_at(repo, commit, path)?;
        if self.by_commit.len() > 4096 {
            self.by_commit.clear();
        }
        self.by_commit.insert(commit, found);
        Ok(found)
    }
}
