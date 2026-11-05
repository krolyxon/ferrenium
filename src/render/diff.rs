//! Commit diffs: the cached, render-ready model behind the commit page,
//! and plain-text patches.

use std::fmt::Write;
use std::sync::Arc;

use crate::cache::Cache;
use crate::git::diff::{self, Changes, FileChange, LineKind, Side, Status};
use crate::git::tree::{self, EntryKind};
use crate::git::{CommitDetail, Handle, ObjectId};
use crate::render::highlight;

/// Knobs for [`build`]; everything that changes the output is part of the
/// cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Options {
    pub context: u32,
    pub ignore_ws: bool,
    /// Above this many files only the file list is shown.
    pub max_files: usize,
    /// Changed lines rendered for the whole commit, and per file.
    pub max_lines: usize,
    pub max_file_lines: usize,
    /// Blobs larger than this are not diffed.
    pub max_blob_bytes: u64,
    pub max_highlight_bytes: u64,
}

pub struct CommitDiff {
    pub files: Vec<FileDiff>,
    /// Totals over the files that were diffed.
    pub added: u64,
    pub removed: u64,
    /// More than `max_files` files: no per-file diffs at all.
    pub too_many_files: bool,
}

impl CommitDiff {
    pub fn weight(&self) -> usize {
        64 + self.files.iter().map(FileDiff::weight).sum::<usize>()
    }
}

pub struct FileDiff {
    pub change: FileChange,
    /// Lines added and removed; `None` when the file wasn't diffed.
    pub stat: Option<(u32, u32)>,
    /// `(old, new)` modes when only the mode or type changed alongside.
    pub mode_change: Option<(&'static str, &'static str)>,
    pub body: Body,
}

impl FileDiff {
    fn weight(&self) -> usize {
        let body = match &self.body {
            Body::Hunks(hunks) => hunks
                .iter()
                .map(|h| h.header.len() + h.lines.iter().map(|l| l.html.len() + 32).sum::<usize>())
                .sum(),
            _ => 0,
        };
        128 + self.change.path().len() * 2 + body
    }
}

pub enum Body {
    Hunks(Vec<Hunk>),
    /// Content unchanged (pure rename, mode change, whitespace-only with
    /// `ignore_ws`).
    Unchanged,
    Binary,
    Submodule {
        old: Option<ObjectId>,
        new: Option<ObjectId>,
    },
    /// Over a size limit: the blob, the per-file or the per-commit lines.
    TooLarge,
    /// Not diffed because the commit has too many files.
    NotShown,
}

pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
}

pub struct Line {
    pub kind: LineKind,
    /// 1-based line numbers.
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub html: String,
}

impl Line {
    pub fn sign(&self) -> &'static str {
        match self.kind {
            LineKind::Context => " ",
            LineKind::Added => "+",
            LineKind::Removed => "\u{2212}",
        }
    }

    pub fn class(&self) -> &'static str {
        match self.kind {
            LineKind::Context => "ctx",
            LineKind::Added => "add",
            LineKind::Removed => "del",
        }
    }
}

enum Content {
    Text(String),
    Binary,
    TooLarge,
    Missing,
}

impl Content {
    fn text(&self) -> &str {
        match self {
            Content::Text(t) => t,
            _ => "",
        }
    }
}

fn load(repo: &Handle, side: Option<&Side>, max_bytes: Option<u64>) -> anyhow::Result<Content> {
    let Some(side) = side else {
        return Ok(Content::Missing);
    };
    if let Some(max) = max_bytes
        && tree::size(repo, side.oid)? > max
    {
        return Ok(Content::TooLarge);
    }
    let data = tree::blob(repo, side.oid)?;
    if data[..data.len().min(8000)].contains(&0) {
        return Ok(Content::Binary);
    }
    Ok(Content::Text(String::from_utf8_lossy(&data).into_owned()))
}

fn split(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn mode_change(change: &FileChange) -> Option<(&'static str, &'static str)> {
    let (old, new) = (change.old.as_ref()?, change.new.as_ref()?);
    (old.kind != new.kind).then(|| (diff::mode(old.kind), diff::mode(new.kind)))
}

fn is_submodule(side: Option<&Side>) -> bool {
    submodule(side).is_some()
}

fn submodule(side: Option<&Side>) -> Option<&Side> {
    side.filter(|s| s.kind == EntryKind::Submodule)
}

/// Diff every changed file, highlighting both sides as whole files.
pub fn build(
    repo: &Handle,
    cache: &Cache,
    changes: &Changes,
    options: &Options,
) -> anyhow::Result<CommitDiff> {
    let too_many_files = changes.files.len() > options.max_files;
    let mut out = CommitDiff {
        files: Vec::with_capacity(changes.files.len()),
        added: 0,
        removed: 0,
        too_many_files,
    };
    let mut budget = options.max_lines;
    for change in &changes.files {
        let mut file = FileDiff {
            change: change.clone(),
            stat: None,
            mode_change: mode_change(change),
            body: Body::NotShown,
        };
        if too_many_files {
            out.files.push(file);
            continue;
        }
        let (old_side, new_side) = (change.old.as_ref(), change.new.as_ref());
        if is_submodule(old_side) || is_submodule(new_side) {
            file.body = Body::Submodule {
                old: submodule(old_side).map(|s| s.oid),
                new: submodule(new_side).map(|s| s.oid),
            };
            out.files.push(file);
            continue;
        }
        if old_side.map(|s| s.oid) == new_side.map(|s| s.oid) {
            file.stat = Some((0, 0));
            file.body = Body::Unchanged;
            out.files.push(file);
            continue;
        }
        let max = Some(options.max_blob_bytes);
        let (old, new) = (load(repo, old_side, max)?, load(repo, new_side, max)?);
        file.body = match (&old, &new) {
            (Content::TooLarge, _) | (_, Content::TooLarge) => Body::TooLarge,
            (Content::Binary, _) | (_, Content::Binary) => Body::Binary,
            _ => {
                let (old_text, new_text) = (old.text(), new.text());
                let (old_lines, new_lines) = (split(old_text), split(new_text));
                let lines = diff::lines(&old_lines, &new_lines, options.context, options.ignore_ws);
                file.stat = Some((lines.added, lines.removed));
                out.added += u64::from(lines.added);
                out.removed += u64::from(lines.removed);
                let changed = (lines.added + lines.removed) as usize;
                if lines.hunks.is_empty() {
                    Body::Unchanged
                } else if changed > options.max_file_lines || changed > budget {
                    Body::TooLarge
                } else {
                    budget -= changed;
                    let html = |side: Option<&Side>, text: &str| match side {
                        Some(side) => {
                            highlight::cached_lines(
                                cache,
                                side.oid,
                                &String::from_utf8_lossy(&side.path),
                                text,
                                options.max_highlight_bytes,
                            )
                            .0
                        }
                        None => Arc::new(Vec::new()),
                    };
                    let old_html = html(old_side, old_text);
                    let new_html = html(new_side, new_text);
                    let get = |lines: &[String], i: Option<u32>| {
                        i.and_then(|i| lines.get(i as usize))
                            .cloned()
                            .unwrap_or_default()
                    };
                    Body::Hunks(
                        lines
                            .hunks
                            .iter()
                            .map(|h| Hunk {
                                header: h.header(),
                                lines: h
                                    .lines
                                    .iter()
                                    .map(|l| Line {
                                        kind: l.kind,
                                        old: l.old.map(|n| n + 1),
                                        new: l.new.map(|n| n + 1),
                                        html: match l.kind {
                                            LineKind::Removed => get(&old_html, l.old),
                                            _ => get(&new_html, l.new),
                                        },
                                    })
                                    .collect(),
                            })
                            .collect(),
                    )
                }
            }
        };
        out.files.push(file);
    }
    Ok(out)
}

/// A `git format-patch`-style patch of `commit` against its first parent.
pub fn patch(repo: &Handle, commit: &CommitDetail, changes: &Changes) -> anyhow::Result<String> {
    let mut stat = String::new();
    let mut body = String::new();
    let (mut files, mut added, mut removed) = (0, 0, 0);
    let width = changes
        .files
        .iter()
        .map(|f| display_path(f).chars().count())
        .max()
        .unwrap_or(0);
    let mut stats = Vec::new();
    for change in &changes.files {
        files += 1;
        let (a, r, bin) = file_patch(repo, change, &mut body)?;
        added += a;
        removed += r;
        stats.push((display_path(change), a, r, bin));
    }
    let max = stats
        .iter()
        .map(|(_, a, r, _)| a + r)
        .max()
        .unwrap_or(0)
        .max(1);
    for (path, a, r, bin) in &stats {
        let _ = write!(stat, " {path:<width$} | ");
        match bin {
            Some(sizes) => {
                let _ = writeln!(stat, "{sizes}");
            }
            None => {
                // Scale the bar like git (to at most 50 columns).
                let scale = |n: u32| -> usize {
                    if max <= 50 {
                        n as usize
                    } else {
                        ((n as u64 * 50).div_ceil(max as u64)) as usize
                    }
                };
                let _ = writeln!(
                    stat,
                    "{} {}{}",
                    a + r,
                    "+".repeat(scale(*a)),
                    "-".repeat(scale(*r))
                );
            }
        }
    }
    let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut summary = format!(" {} changed", plural(files, "file", "files"));
    if added > 0 || removed == 0 {
        let _ = write!(summary, ", {}(+)", plural(added, "insertion", "insertions"));
    }
    if removed > 0 {
        let _ = write!(summary, ", {}(-)", plural(removed, "deletion", "deletions"));
    }

    let (subject, rest) = commit
        .message
        .split_once('\n')
        .unwrap_or((commit.message.as_str(), ""));
    let mut out = String::new();
    let _ = writeln!(out, "From {} Mon Sep 17 00:00:00 2001", commit.info.id);
    let _ = writeln!(
        out,
        "From: {} <{}>",
        commit.author.name, commit.author.email
    );
    let _ = writeln!(
        out,
        "Date: {}",
        crate::render::time::rfc2822(commit.author.time, commit.author.offset)
    );
    let _ = writeln!(out, "Subject: [PATCH] {}", subject.trim());
    out.push('\n');
    let rest = rest.trim();
    if !rest.is_empty() {
        out.push_str(rest);
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(&stat);
    out.push_str(&summary);
    out.push_str("\n\n");
    out.push_str(&body);
    let _ = write!(out, "-- \nferrenium {}\n\n", env!("CARGO_PKG_VERSION"));
    Ok(out)
}

fn display_path(change: &FileChange) -> String {
    let new = String::from_utf8_lossy(change.path()).into_owned();
    match (&change.status, &change.old) {
        (Status::Renamed | Status::Copied, Some(old)) => {
            format!("{} => {new}", String::from_utf8_lossy(&old.path))
        }
        _ => new,
    }
}

fn short(oid: &ObjectId) -> String {
    oid.to_hex_with_len(7).to_string()
}

/// Append one file's `diff --git` section; returns lines added, removed,
/// and the stat text for binary files.
fn file_patch(
    repo: &Handle,
    change: &FileChange,
    out: &mut String,
) -> anyhow::Result<(u32, u32, Option<String>)> {
    let path = |s: Option<&Side>| s.map(|s| String::from_utf8_lossy(&s.path).into_owned());
    let (old, new) = (change.old.as_ref(), change.new.as_ref());
    let a = path(old).unwrap_or_else(|| path(new).unwrap_or_default());
    let b = path(new).unwrap_or_else(|| a.clone());
    let _ = writeln!(out, "diff --git a/{a} b/{b}");
    match (old, new) {
        (None, Some(n)) => {
            let _ = writeln!(out, "new file mode {}", diff::mode(n.kind));
        }
        (Some(o), None) => {
            let _ = writeln!(out, "deleted file mode {}", diff::mode(o.kind));
        }
        (Some(o), Some(n)) => {
            if o.kind != n.kind {
                let _ = writeln!(out, "old mode {}", diff::mode(o.kind));
                let _ = writeln!(out, "new mode {}", diff::mode(n.kind));
            }
            if let (Some(similarity), true) = (
                change.similarity,
                matches!(change.status, Status::Renamed | Status::Copied),
            ) {
                let verb = if change.status == Status::Copied {
                    "copy"
                } else {
                    "rename"
                };
                let _ = writeln!(out, "similarity index {similarity}%");
                let _ = writeln!(out, "{verb} from {a}");
                let _ = writeln!(out, "{verb} to {b}");
            }
        }
        (None, None) => {}
    }
    let old_id = old.map_or_else(|| "0000000".to_string(), |s| short(&s.oid));
    let new_id = new.map_or_else(|| "0000000".to_string(), |s| short(&s.oid));
    if old.map(|s| s.oid) == new.map(|s| s.oid) {
        return Ok((0, 0, None));
    }
    match (old, new) {
        (Some(o), Some(n)) if o.kind == n.kind => {
            let _ = writeln!(out, "index {old_id}..{new_id} {}", diff::mode(n.kind));
        }
        _ => {
            let _ = writeln!(out, "index {old_id}..{new_id}");
        }
    }
    let minus = old.map_or_else(|| "/dev/null".to_string(), |_| format!("a/{a}"));
    let plus = new.map_or_else(|| "/dev/null".to_string(), |_| format!("b/{b}"));

    if is_submodule(old) || is_submodule(new) {
        let _ = writeln!(out, "--- {minus}\n+++ {plus}");
        let (o, n) = (submodule(old), submodule(new));
        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            if o.is_some() { "1" } else { "0,0" },
            if n.is_some() { "1" } else { "0,0" }
        );
        if let Some(o) = o {
            let _ = writeln!(out, "-Subproject commit {}", o.oid);
        }
        if let Some(n) = n {
            let _ = writeln!(out, "+Subproject commit {}", n.oid);
        }
        return Ok((u32::from(n.is_some()), u32::from(o.is_some()), None));
    }

    let (old_c, new_c) = (load(repo, old, None)?, load(repo, new, None)?);
    if matches!(old_c, Content::Binary) || matches!(new_c, Content::Binary) {
        let _ = writeln!(out, "Binary files {minus} and {plus} differ");
        let size = |s: Option<&Side>| -> anyhow::Result<u64> {
            s.map_or(Ok(0), |s| tree::size(repo, s.oid))
        };
        let sizes = format!("Bin {} -> {} bytes", size(old)?, size(new)?);
        return Ok((0, 0, Some(sizes)));
    }
    let (old_l, new_l) = (split(old_c.text()), split(new_c.text()));
    let lines = diff::lines(&old_l, &new_l, 3, false);
    // Like git, an empty file added or removed has no `---`/`+++` lines.
    if lines.hunks.is_empty() {
        return Ok((0, 0, None));
    }
    let _ = writeln!(out, "--- {minus}\n+++ {plus}");
    for hunk in &lines.hunks {
        out.push_str(&hunk.header());
        out.push('\n');
        for line in &hunk.lines {
            let (sign, text) = match line.kind {
                LineKind::Context => (' ', old_l[line.old.unwrap_or(0) as usize]),
                LineKind::Removed => ('-', old_l[line.old.unwrap_or(0) as usize]),
                LineKind::Added => ('+', new_l[line.new.unwrap_or(0) as usize]),
            };
            out.push(sign);
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    Ok((lines.added, lines.removed, None))
}
