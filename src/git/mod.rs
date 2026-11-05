//! All repository access goes through this module; nothing else touches gix.

pub mod diff;
pub mod log;
pub mod refs;
pub mod tree;

use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Context;
use gix::bstr::ByteSlice;

pub use gix::ObjectId;
pub use gix::Repository as Handle;
pub use gix::ThreadSafeRepository;

/// A handle for use on the current thread.
pub fn handle(repo: &ThreadSafeRepository) -> Handle {
    repo.to_thread_local()
}

/// Open a repository by its git dir (the bare repo, or a worktree's `.git`).
///
/// Only the repository's own config is read: no system/global config and no
/// `GIT_*` environment, so behavior doesn't depend on who runs the server.
pub fn open(git_dir: &Path) -> anyhow::Result<ThreadSafeRepository> {
    if git_dir.join("reftable").is_dir() {
        anyhow::bail!("uses the reftable ref format, which is not supported");
    }
    let options = gix::open::Options::isolated().open_path_as_is(true);
    let repo = ThreadSafeRepository::open_opts(git_dir, options)
        .with_context(|| format!("cannot open {}", git_dir.display()))?;
    let repo_local = repo.to_thread_local();
    if let Some(storage) = repo_local.config_snapshot().string("extensions.refStorage")
        && storage != "files"
    {
        anyhow::bail!("uses the {storage} ref format, which is not supported");
    }
    Ok(repo)
}

/// Repository metadata stored in its git config.
///
/// `ferrenium.*` keys win; `gitweb.*` and `cgit.*` are read for painless
/// migration from those tools.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigMeta {
    pub description: Option<String>,
    pub owner: Option<String>,
    pub section: Option<String>,
    pub hidden: Option<bool>,
    pub readme: Option<String>,
    pub default_branch: Option<String>,
}

pub fn config_meta(repo: &gix::Repository) -> ConfigMeta {
    let config = repo.config_snapshot();
    let string = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            let value = config.string(*key)?;
            let value = value.to_str_lossy().trim().to_string();
            (!value.is_empty()).then_some(value)
        })
    };
    ConfigMeta {
        description: string(&["ferrenium.description", "gitweb.description", "cgit.desc"]),
        owner: string(&["ferrenium.owner", "gitweb.owner", "cgit.owner"]),
        section: string(&["ferrenium.section", "gitweb.category", "cgit.section"]),
        hidden: ["ferrenium.hidden", "cgit.hide"]
            .iter()
            .find_map(|key| config.boolean(*key)),
        // cgit allows `branch:path`; we always read from the default branch.
        readme: string(&["ferrenium.readme", "cgit.readme"])
            .map(|r| {
                r.rsplit_once(':')
                    .map_or(r.clone(), |(_, path)| path.to_string())
            })
            .filter(|r| !r.is_empty()),
        default_branch: string(&["ferrenium.defaultBranch", "cgit.defbranch"]),
    }
}

/// Short branch name HEAD points to (also when that branch is unborn).
pub fn head_branch(repo: &gix::Repository) -> Option<String> {
    let name = repo.head_name().ok()??;
    name.as_bstr()
        .strip_prefix(b"refs/heads/")
        .map(|b| b.to_str_lossy().into_owned())
}

/// Counts and a fingerprint over all refs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefSummary {
    pub branches: usize,
    pub tags: usize,
    /// Newest committer time across branch tips (unix seconds).
    pub last_activity: Option<i64>,
    /// Changes whenever any ref (or HEAD) changes.
    pub fingerprint: u64,
}

pub fn ref_summary(repo: &gix::Repository) -> anyhow::Result<RefSummary> {
    let mut summary = RefSummary::default();
    let mut hasher = std::hash::DefaultHasher::new();
    if let Ok(head) = repo.find_reference("HEAD") {
        format!("{:?}", head.target()).hash(&mut hasher);
    }
    let platform = repo.references().context("cannot read refs")?;
    for reference in platform.all().context("cannot read refs")? {
        let Ok(mut reference) = reference else {
            continue;
        };
        let name = reference.name().as_bstr().to_owned();
        name.hash(&mut hasher);
        format!("{:?}", reference.target()).hash(&mut hasher);

        if name.starts_with(b"refs/heads/") {
            summary.branches += 1;
            if let Ok(commit) = reference.peel_to_commit()
                && let Ok(time) = commit.time()
            {
                summary.last_activity = summary.last_activity.max(Some(time.seconds));
            }
        } else if name.starts_with(b"refs/tags/") {
            summary.tags += 1;
        }
    }
    summary.fingerprint = hasher.finish();
    Ok(summary)
}

/// Commit id a branch points to, if the branch exists.
pub fn branch_commit(repo: &gix::Repository, branch: &str) -> Option<ObjectId> {
    let mut reference = repo.find_reference(&format!("refs/heads/{branch}")).ok()?;
    Some(reference.peel_to_commit().ok()?.id)
}

/// Names tried for the README, in order of preference (case-insensitive).
const README_NAMES: &[&str] = &[
    "readme.md",
    "readme.markdown",
    "readme",
    "readme.txt",
    "readme.rst",
    "readme.adoc",
    "readme.org",
];

/// Locate the README blob in `commit`'s tree: `preferred` (a path) if given,
/// otherwise the best match among root entries.
pub fn find_readme(
    repo: &gix::Repository,
    commit: ObjectId,
    preferred: Option<&str>,
) -> Option<(String, ObjectId)> {
    let tree = repo.find_commit(commit).ok()?.tree().ok()?;
    if let Some(path) = preferred {
        let entry = tree.lookup_entry_by_path(path).ok()??;
        return entry
            .mode()
            .is_blob()
            .then(|| (path.to_string(), entry.object_id()));
    }
    let mut best: Option<(usize, String, ObjectId)> = None;
    for entry in tree.iter().flatten() {
        if !entry.mode().is_blob() {
            continue;
        }
        let name = entry.filename().to_str_lossy();
        let lower = name.to_ascii_lowercase();
        if let Some(rank) = README_NAMES.iter().position(|n| *n == lower)
            && best.as_ref().is_none_or(|(r, _, _)| rank < *r)
        {
            best = Some((rank, name.into_owned(), entry.oid().to_owned()));
        }
    }
    best.map(|(_, name, oid)| (name, oid))
}

/// The parts of a commit shown in listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    /// Unix seconds.
    pub author_time: i64,
    pub committer_time: i64,
}

impl CommitInfo {
    pub fn weight(&self) -> usize {
        64 + self.subject.len() + self.author_name.len() + self.author_email.len()
    }
}

pub fn commit_info(repo: &gix::Repository, id: ObjectId) -> anyhow::Result<CommitInfo> {
    let commit = repo.find_commit(id).context("commit not found")?;
    let decoded = commit.decode().context("malformed commit")?;
    let author = decoded.author().context("malformed author")?;
    let committer = decoded.committer().context("malformed committer")?;
    Ok(CommitInfo {
        id,
        subject: decoded
            .message()
            .summary()
            .to_str_lossy()
            .trim()
            .to_string(),
        author_name: author.name.to_str_lossy().trim().to_string(),
        author_email: author.email.to_str_lossy().trim().to_string(),
        author_time: author.seconds(),
        committer_time: committer.seconds(),
    })
}

/// Number of commits reachable from `tip` (uses the commit-graph when the
/// repository has one).
pub fn commit_count(repo: &gix::Repository, tip: ObjectId) -> anyhow::Result<u64> {
    let mut count = 0;
    for info in repo.rev_walk([tip]).all()? {
        info?;
        count += 1;
    }
    Ok(count)
}

/// Everything the commit page shows about a commit.
#[derive(Debug, Clone)]
pub struct CommitDetail {
    pub info: CommitInfo,
    /// The message after the subject line, trimmed.
    pub body: String,
    /// The full message as stored, for patches.
    pub message: String,
    pub author: Person,
    pub committer: Person,
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
    /// Unix seconds and the UTC offset in seconds, as recorded.
    pub time: i64,
    pub offset: i32,
}

pub fn commit_detail(repo: &gix::Repository, id: ObjectId) -> anyhow::Result<CommitDetail> {
    let commit = repo.find_commit(id).context("commit not found")?;
    let decoded = commit.decode().context("malformed commit")?;
    let person = |sig: gix::actor::SignatureRef<'_>| {
        let time = sig.time().unwrap_or_default();
        Person {
            name: sig.name.to_str_lossy().trim().to_string(),
            email: sig.email.to_str_lossy().trim().to_string(),
            time: time.seconds,
            offset: time.offset,
        }
    };
    let message = decoded.message();
    Ok(CommitDetail {
        info: commit_info(repo, id)?,
        body: message
            .body()
            .map(|b| b.to_str_lossy().trim().to_string())
            .unwrap_or_default(),
        message: decoded.message.to_str_lossy().into_owned(),
        author: person(decoded.author().context("malformed author")?),
        committer: person(decoded.committer().context("malformed committer")?),
        parents: decoded.parents().collect(),
        tree: decoded.tree(),
    })
}

/// A `.mailmap`: canonical names and emails for authors.
#[derive(Default)]
pub struct Mailmap(gix::mailmap::Snapshot);

impl Mailmap {
    pub fn parse(data: &[u8]) -> Self {
        Mailmap(gix::mailmap::Snapshot::from_bytes(data))
    }

    /// Canonical `(name, email)` for an identity.
    pub fn resolve(&self, name: &str, email: &str) -> (String, String) {
        let signature = gix::actor::SignatureRef {
            name: name.as_bytes().as_bstr(),
            email: email.as_bytes().as_bstr(),
            time: "",
        };
        match self.0.try_resolve_ref(signature) {
            Some(found) => (
                found
                    .name
                    .map_or_else(|| name.to_string(), |n| n.to_str_lossy().into_owned()),
                found
                    .email
                    .map_or_else(|| email.to_string(), |e| e.to_str_lossy().into_owned()),
            ),
            None => (name.to_string(), email.to_string()),
        }
    }

    pub fn weight(&self) -> usize {
        self.0
            .iter()
            .map(|e| {
                64 + e.old_email().len()
                    + e.old_name().map_or(0, |n| n.len())
                    + e.new_email().map_or(0, |n| n.len())
                    + e.new_name().map_or(0, |n| n.len())
            })
            .sum()
    }
}

/// The `.mailmap` blob at the root of `commit`, if there is one.
pub fn mailmap_blob(repo: &gix::Repository, commit: ObjectId) -> Option<ObjectId> {
    let tree = repo.find_commit(commit).ok()?.tree().ok()?;
    let entry = tree.find_entry(".mailmap")?;
    entry.mode().is_blob().then(|| entry.object_id())
}
