//! Resolving revisions from URLs and listing refs.

use gix::bstr::{BStr, ByteSlice};

use super::{CommitInfo, ObjectId, commit_info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
    /// A (possibly abbreviated) commit id written in the URL.
    Commit,
}

/// A revision named in a URL, resolved to a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    /// The name as written in the URL (`main`, `feature/x`, `v1.0`, an oid).
    pub name: String,
    pub kind: RefKind,
    pub commit: ObjectId,
}

impl Rev {
    /// URLs naming a full commit id always show the same content.
    pub fn is_immutable(&self) -> bool {
        self.kind == RefKind::Commit && self.name.len() == self.commit.kind().len_in_hex()
    }
}

/// Resolve `name` to a commit: a branch, then a tag, then a hex commit id
/// (at least 4 digits).
pub fn resolve(repo: &gix::Repository, name: &str) -> Option<Rev> {
    if name.is_empty() || name.starts_with('-') {
        return None;
    }
    for (prefix, kind) in [
        ("refs/heads/", RefKind::Branch),
        ("refs/tags/", RefKind::Tag),
    ] {
        if let Ok(mut reference) = repo.find_reference(&format!("{prefix}{name}"))
            && let Ok(commit) = reference.peel_to_commit()
        {
            return Some(Rev {
                name: name.to_string(),
                kind,
                commit: commit.id,
            });
        }
    }
    if (4..=40).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        let id = repo.rev_parse_single(BStr::new(name)).ok()?;
        let commit = id.object().ok()?.peel_to_commit().ok()?;
        return Some(Rev {
            name: name.to_string(),
            kind: RefKind::Commit,
            commit: commit.id,
        });
    }
    None
}

/// Resolve the longest prefix of `segments` that names a revision; the
/// remaining segments are the path. Ref names may contain `/`.
pub fn resolve_prefix<'a>(
    repo: &gix::Repository,
    segments: &'a [Vec<u8>],
) -> Option<(Rev, &'a [Vec<u8>])> {
    (1..=segments.len()).rev().find_map(|n| {
        let parts: Option<Vec<&str>> = segments[..n]
            .iter()
            .map(|s| std::str::from_utf8(s).ok())
            .collect();
        let rev = resolve(repo, &parts?.join("/"))?;
        Some((rev, &segments[n..]))
    })
}

#[derive(Debug, Clone)]
pub struct RefEntry {
    /// Short name (`main`, `v1.0`).
    pub name: String,
    pub commit: ObjectId,
    pub info: CommitInfo,
    /// Tag message subject, for annotated tags.
    pub tag_subject: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RefList {
    /// Newest commit first.
    pub branches: Vec<RefEntry>,
    /// Newest first.
    pub tags: Vec<RefEntry>,
}

impl RefList {
    pub fn weight(&self) -> usize {
        let entry = |e: &RefEntry| {
            64 + e.name.len() + e.info.weight() + e.tag_subject.as_ref().map_or(0, String::len)
        };
        self.branches.iter().chain(&self.tags).map(entry).sum()
    }
}

/// Every branch and tag that points (possibly through tags) at a commit.
pub fn list(repo: &gix::Repository) -> anyhow::Result<RefList> {
    let mut list = RefList::default();
    let platform = repo.references()?;
    for reference in platform.all()? {
        let Ok(mut reference) = reference else {
            continue;
        };
        let full = reference.name().as_bstr().to_owned();
        let (name, is_tag) = if let Some(n) = full.strip_prefix(b"refs/heads/") {
            (n, false)
        } else if let Some(n) = full.strip_prefix(b"refs/tags/") {
            (n, true)
        } else {
            continue;
        };
        let Ok(name) = name.to_str() else {
            continue;
        };
        let tag_subject = is_tag
            .then(|| {
                let id = reference.target().try_id()?.to_owned();
                let tag = repo.find_object(id).ok()?.try_into_tag().ok()?;
                let tag = tag.decode().ok()?;
                let subject = tag
                    .message
                    .lines()
                    .next()?
                    .to_str_lossy()
                    .trim()
                    .to_string();
                (!subject.is_empty()).then_some(subject)
            })
            .flatten();
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        let Ok(info) = commit_info(repo, commit.id) else {
            continue;
        };
        let entry = RefEntry {
            name: name.to_string(),
            commit: commit.id,
            info,
            tag_subject,
        };
        if is_tag {
            list.tags.push(entry);
        } else {
            list.branches.push(entry);
        }
    }
    let newest_first = |a: &RefEntry, b: &RefEntry| {
        b.info
            .committer_time
            .cmp(&a.info.committer_time)
            .then_with(|| a.name.cmp(&b.name))
    };
    list.branches.sort_by(newest_first);
    list.tags.sort_by(newest_first);
    Ok(list)
}
