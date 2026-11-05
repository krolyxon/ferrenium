//! Trees and blobs at a commit.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::Context;
use gix::bstr::{BString, ByteSlice};
use gix::object::tree::EntryKind as GixKind;

use super::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tree,
    Blob,
    Executable,
    Symlink,
    Submodule,
}

impl EntryKind {
    fn from_gix(kind: GixKind) -> Self {
        match kind {
            GixKind::Tree => EntryKind::Tree,
            GixKind::Blob => EntryKind::Blob,
            GixKind::BlobExecutable => EntryKind::Executable,
            GixKind::Link => EntryKind::Symlink,
            GixKind::Commit => EntryKind::Submodule,
        }
    }

    pub fn is_blob(self) -> bool {
        matches!(
            self,
            EntryKind::Blob | EntryKind::Executable | EntryKind::Symlink
        )
    }
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: BString,
    pub kind: EntryKind,
    pub oid: ObjectId,
    /// Blob size in bytes (blobs and symlinks only).
    pub size: Option<u64>,
}

/// What a path inside a commit's tree points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTarget {
    Tree(ObjectId),
    Blob(ObjectId, EntryKind),
    Submodule(ObjectId),
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// Look up `path` (slash-separated, empty for the root) in `commit`'s tree.
pub fn lookup(
    repo: &gix::Repository,
    commit: ObjectId,
    path: &[u8],
) -> anyhow::Result<Option<PathTarget>> {
    let tree = repo
        .find_commit(commit)
        .context("commit not found")?
        .tree()
        .context("commit has no tree")?;
    if path.is_empty() {
        return Ok(Some(PathTarget::Tree(tree.id)));
    }
    let Some(entry) = tree.lookup_entry_by_path(as_path(path))? else {
        return Ok(None);
    };
    let oid = entry.object_id();
    Ok(Some(match EntryKind::from_gix(entry.mode().kind()) {
        EntryKind::Tree => PathTarget::Tree(oid),
        EntryKind::Submodule => PathTarget::Submodule(oid),
        kind => PathTarget::Blob(oid, kind),
    }))
}

/// Entries of a tree: directories first, then files, each by name.
pub fn list(repo: &gix::Repository, tree: ObjectId) -> anyhow::Result<Vec<TreeEntry>> {
    let tree = repo.find_tree(tree).context("tree not found")?;
    let mut entries = Vec::new();
    for entry in tree.iter() {
        let entry = entry?;
        let kind = EntryKind::from_gix(entry.mode().kind());
        let oid = entry.oid().to_owned();
        let size = kind
            .is_blob()
            .then(|| repo.find_header(oid).ok().map(|h| h.size()))
            .flatten();
        entries.push(TreeEntry {
            name: entry.filename().to_owned(),
            kind,
            oid,
            size,
        });
    }
    entries.sort_by(|a, b| {
        let dir = |e: &TreeEntry| e.kind != EntryKind::Tree;
        dir(a)
            .cmp(&dir(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Full contents of a blob.
pub fn blob(repo: &gix::Repository, oid: ObjectId) -> anyhow::Result<Vec<u8>> {
    let mut blob = repo.find_blob(oid).context("blob not found")?;
    Ok(blob.take_data())
}

/// Size of an object without reading its contents.
pub fn size(repo: &gix::Repository, oid: ObjectId) -> anyhow::Result<u64> {
    Ok(repo.find_header(oid)?.size())
}
