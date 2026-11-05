//! Snapshots of a commit's tree as `.tar.gz` or `.zip`.

use std::io::{self, Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;

use anyhow::Context;

use super::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    TarGz,
    Zip,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::TarGz => "tar.gz",
            Format::Zip => "zip",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::TarGz => "application/gzip",
            Format::Zip => "application/zip",
        }
    }
}

/// Write the tree of `commit` to `out`, every path under `prefix` (which
/// should end in `/`), with the commit's time as every file's mtime.
pub fn write(
    repo: &gix::Repository,
    commit: ObjectId,
    format: Format,
    prefix: &str,
    out: impl Write,
) -> anyhow::Result<()> {
    let commit = repo.find_commit(commit).context("commit not found")?;
    let tree = commit.tree_id().context("commit has no tree")?.detach();
    let mtime = commit.time().context("malformed commit")?.seconds;
    let (stream, _index) = repo.worktree_stream(tree).context("cannot read the tree")?;
    let options = gix::worktree::archive::Options {
        format: match format {
            Format::TarGz => gix::worktree::archive::Format::TarGz {
                compression_level: Some(6),
            },
            Format::Zip => gix::worktree::archive::Format::Zip {
                compression_level: Some(6),
            },
        },
        tree_prefix: Some(prefix.into()),
        modification_time: mtime,
    };
    repo.worktree_archive(
        stream,
        Positioned { inner: out, pos: 0 },
        gix::progress::Discard,
        &AtomicBool::new(false),
        options,
    )
    .context("cannot write archive")?;
    Ok(())
}

/// A writer that can report its position but not move. The zip writer
/// only asks where it is, so archives stream without a temporary file.
struct Positioned<W> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Positioned<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W> Seek for Positioned<W> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match to {
            SeekFrom::Current(0) => Ok(self.pos),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "archive output is a stream",
            )),
        }
    }
}
