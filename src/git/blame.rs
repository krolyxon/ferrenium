//! Line attribution: which commit last touched each line of a file.

use anyhow::Context;
use gix::bstr::{BStr, ByteSlice};

use super::ObjectId;

/// A run of consecutive lines last changed by the same commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameHunk {
    /// 0-based first line in the blamed file, and the number of lines.
    pub start: u32,
    pub len: u32,
    pub commit: ObjectId,
    /// 0-based first line in that commit's version of the file.
    pub source_start: u32,
    /// The file's path in that commit, when it was renamed since.
    pub source_path: Option<String>,
}

/// Blame `path` as of `commit`, following renames like `git blame`.
/// Hunks are in file order and cover every line.
pub fn blame(
    repo: &gix::Repository,
    commit: ObjectId,
    path: &[u8],
) -> anyhow::Result<Vec<BlameHunk>> {
    let options = gix::repository::blame_file::Options {
        diff_algorithm: Some(gix::diff::blob::Algorithm::Myers),
        rewrites: Some(Default::default()),
        ..Default::default()
    };
    let outcome = repo
        .blame_file(BStr::new(path), commit, options)
        .context("blame failed")?;
    let mut hunks: Vec<BlameHunk> = outcome
        .entries
        .iter()
        .map(|e| BlameHunk {
            start: e.start_in_blamed_file,
            len: e.len.get(),
            commit: e.commit_id,
            source_start: e.start_in_source_file,
            source_path: e
                .source_file_name
                .as_ref()
                .map(|p| p.to_str_lossy().into_owned()),
        })
        .collect();
    hunks.sort_by_key(|h| h.start);
    // Adjacent entries from the same commit read as one hunk.
    let mut merged: Vec<BlameHunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        match merged.last_mut() {
            Some(last)
                if last.commit == hunk.commit
                    && last.start + last.len == hunk.start
                    && last.source_start + last.len == hunk.source_start
                    && last.source_path == hunk.source_path =>
            {
                last.len += hunk.len;
            }
            _ => merged.push(hunk),
        }
    }
    Ok(merged)
}
