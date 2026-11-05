//! What a commit changed: the file list (with renames) against its first
//! parent, and line diffs grouped into hunks.

use std::hash::Hash;

use anyhow::Context;
use gix::bstr::BString;
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::object::tree::diff::ChangeDetached;

use super::ObjectId;
use super::tree::EntryKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

/// One side of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub path: BString,
    pub oid: ObjectId,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub status: Status,
    pub old: Option<Side>,
    pub new: Option<Side>,
    /// Rename/copy similarity in percent.
    pub similarity: Option<u8>,
}

impl FileChange {
    /// The path after the change (before it, for deletions).
    pub fn path(&self) -> &BString {
        match (&self.new, &self.old) {
            (Some(side), _) | (None, Some(side)) => &side.path,
            (None, None) => unreachable!("a change has at least one side"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Changes {
    /// The first parent; `None` for a root commit (compared to nothing).
    pub parent: Option<ObjectId>,
    pub files: Vec<FileChange>,
}

/// Files changed by `commit` relative to its first parent, renames
/// detected like `git diff -M` (50% similarity), sorted by path.
pub fn changes(repo: &gix::Repository, commit: ObjectId) -> anyhow::Result<Changes> {
    let commit = repo.find_commit(commit).context("commit not found")?;
    let new_tree = commit.tree().context("commit has no tree")?;
    let parent = commit.parent_ids().next().map(|id| id.detach());
    let old_tree = match parent {
        Some(id) => Some(
            repo.find_commit(id)
                .context("parent commit not found")?
                .tree()
                .context("parent has no tree")?,
        ),
        None => None,
    };
    let options = gix::diff::Options::default().with_rewrites(Some(Default::default()));
    let changes = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), options)
        .context("cannot diff trees")?;

    let side = |path: &BString, mode: gix::object::tree::EntryMode, oid: ObjectId| {
        (!mode.is_tree()).then(|| Side {
            path: path.clone(),
            oid,
            kind: EntryKind::from_gix(mode.kind()),
        })
    };
    let mut files = Vec::new();
    for change in changes {
        let file = match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } => FileChange {
                status: Status::Added,
                old: None,
                new: side(&location, entry_mode, id),
                similarity: None,
            },
            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => FileChange {
                status: Status::Deleted,
                old: side(&location, entry_mode, id),
                new: None,
                similarity: None,
            },
            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
            } => {
                // A tree replaced by a file (or the reverse) is just an
                // addition (deletion) of the file. A file that changed type
                // (regular, symlink, submodule) is both, as git shows it.
                let old = side(&location, previous_entry_mode, previous_id);
                let new = side(&location, entry_mode, id);
                match (old, new) {
                    (Some(old), Some(new)) if !old.kind.same_type(new.kind) => {
                        files.push(FileChange {
                            status: Status::Deleted,
                            old: Some(old),
                            new: None,
                            similarity: None,
                        });
                        FileChange {
                            status: Status::Added,
                            old: None,
                            new: Some(new),
                            similarity: None,
                        }
                    }
                    (old, new) => FileChange {
                        status: match (&old, &new) {
                            (None, _) => Status::Added,
                            (_, None) => Status::Deleted,
                            _ => Status::Modified,
                        },
                        old,
                        new,
                        similarity: None,
                    },
                }
            }
            ChangeDetached::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                diff,
                entry_mode,
                id,
                location,
                copy,
                ..
            } => FileChange {
                status: if copy {
                    Status::Copied
                } else {
                    Status::Renamed
                },
                old: side(&source_location, source_entry_mode, source_id),
                new: side(&location, entry_mode, id),
                // Truncated like git's percentage.
                similarity: Some(
                    diff.map_or(100.0, |d| d.similarity * 100.0)
                        .floor()
                        .clamp(0.0, 100.0) as u8,
                ),
            },
        };
        if file.old.is_some() || file.new.is_some() {
            files.push(file);
        }
    }
    files.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(Changes { parent, files })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// A line in a hunk; `old`/`new` are 0-based line indexes into each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 0-based first line and length on each side.
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<Line>,
    /// The enclosing "function" line git shows after the ranges.
    pub func: Option<String>,
}

impl Hunk {
    /// `@@ -1,5 +1,8 @@ fn main() {`
    pub fn header(&self) -> String {
        fn range(start: u32, len: u32) -> String {
            match len {
                0 => format!("{start},0"),
                1 => format!("{}", start + 1),
                _ => format!("{},{len}", start + 1),
            }
        }
        let mut header = format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        );
        if let Some(func) = &self.func {
            header.push(' ');
            header.push_str(func);
        }
        header
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineDiff {
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
}

/// Line diff of two texts split into lines (each with its `\n`, so a
/// missing final newline counts as a change), with `context` unchanged
/// lines around each change. `ignore_ws` compares lines with all
/// whitespace removed, like `git diff -w`.
pub fn lines(old: &[&str], new: &[&str], context: u32, ignore_ws: bool) -> LineDiff {
    let diff = if ignore_ws {
        let strip =
            |l: &&str| -> Vec<u8> { l.bytes().filter(|b| !b.is_ascii_whitespace()).collect() };
        compute(old.iter().map(strip), new.iter().map(strip))
    } else {
        compute(
            old.iter().map(|l| l.as_bytes()),
            new.iter().map(|l| l.as_bytes()),
        )
    };
    let raw: Vec<_> = diff.hunks().collect();
    let (old_len, new_len) = (old.len() as u32, new.len() as u32);
    let mut out = LineDiff {
        hunks: Vec::new(),
        added: diff.count_additions(),
        removed: diff.count_removals(),
    };
    let mut i = 0;
    while i < raw.len() {
        // Merge changes whose context would overlap or touch.
        let mut j = i;
        while j + 1 < raw.len() && raw[j + 1].before.start - raw[j].before.end <= 2 * context {
            j += 1;
        }
        let lead = raw[i].before.start.min(context);
        let (old_start, new_start) = (raw[i].before.start - lead, raw[i].after.start - lead);
        let trail = context.min(old_len - raw[j].before.end);
        let (old_end, new_end) = (raw[j].before.end + trail, raw[j].after.end + trail);
        debug_assert!(new_end <= new_len);

        let mut lines = Vec::new();
        let (mut o, mut n) = (old_start, new_start);
        for h in &raw[i..=j] {
            while o < h.before.start {
                lines.push(Line {
                    kind: LineKind::Context,
                    old: Some(o),
                    new: Some(n),
                });
                o += 1;
                n += 1;
            }
            for r in h.before.clone() {
                lines.push(Line {
                    kind: LineKind::Removed,
                    old: Some(r),
                    new: None,
                });
            }
            for a in h.after.clone() {
                lines.push(Line {
                    kind: LineKind::Added,
                    old: None,
                    new: Some(a),
                });
            }
            o = h.before.end;
            n = h.after.end;
        }
        while o < old_end {
            lines.push(Line {
                kind: LineKind::Context,
                old: Some(o),
                new: Some(n),
            });
            o += 1;
            n += 1;
        }
        out.hunks.push(Hunk {
            old_start,
            old_len: old_end - old_start,
            new_start,
            new_len: new_end - new_start,
            lines,
            func: func_line(&old[..old_start as usize]),
        });
        i = j + 1;
    }
    out
}

/// git's default hunk-header context: the closest line above the hunk
/// that starts like an identifier, trailing whitespace trimmed, at most
/// 80 bytes.
fn func_line(before: &[&str]) -> Option<String> {
    let line = before.iter().rev().find(|l| {
        l.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
    })?;
    let bytes = &line.as_bytes()[..line.len().min(80)];
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

fn compute<T: AsRef<[u8]> + Hash + Eq + Default>(
    old: impl Iterator<Item = T>,
    new: impl Iterator<Item = T>,
) -> Diff {
    let mut input = InternedInput::default();
    input.update_before(old);
    input.update_after(new);
    gix::diff::blob::diff_with_slider_heuristics(Algorithm::Myers, &input)
}

/// Git's mode for an entry kind (`100644`, …).
pub fn mode(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Blob => "100644",
        EntryKind::Executable => "100755",
        EntryKind::Symlink => "120000",
        EntryKind::Submodule => "160000",
        EntryKind::Tree => "040000",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<&str> {
        text.split_inclusive('\n').collect()
    }

    #[test]
    fn hunks_with_context_and_merging() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let diff = lines(&split(old), &split(new), 3, false);
        assert_eq!((diff.added, diff.removed), (2, 2));
        // Changes at lines 2 and 10 are 7 apart: with 3 lines of context
        // they stay separate hunks.
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[0].header(), "@@ -1,5 +1,5 @@");
        assert_eq!(diff.hunks[1].header(), "@@ -7,4 +7,4 @@ f");
        // With more context they merge.
        let diff = lines(&split(old), &split(new), 4, false);
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].header(), "@@ -1,10 +1,10 @@");
    }

    #[test]
    fn additions_to_empty_and_whitespace() {
        let diff = lines(&[], &split("x\ny\n"), 3, false);
        assert_eq!(diff.hunks[0].header(), "@@ -0,0 +1,2 @@");
        assert_eq!(diff.hunks[0].lines.len(), 2);

        let old = split("fn x() {\n    a;\n}\n");
        let new = split("fn x() {\n\ta;\n}\n");
        assert_eq!(lines(&old, &new, 3, false).added, 1);
        let ws = lines(&old, &new, 3, true);
        assert!(ws.hunks.is_empty());
        assert_eq!((ws.added, ws.removed), (0, 0));
    }

    #[test]
    fn missing_final_newline_is_a_change() {
        let diff = lines(&split("a\nb\n"), &split("a\nb"), 0, false);
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert_eq!(diff.hunks[0].header(), "@@ -2 +2 @@ a");
    }
}
