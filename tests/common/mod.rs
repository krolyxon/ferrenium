//! Fixture helpers: build real repositories with the git CLI.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run git in `dir` with an isolated environment; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let home = std::env::temp_dir();
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", home)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repo at `root/rel` (created with `init --bare`).
pub fn bare(root: &Path, rel: &str, extra: &[&str]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    let mut args = vec!["init", "--bare", "-q", "-b", "main"];
    args.extend_from_slice(extra);
    git(&path, &args);
    path
}

/// A scratch clone of `bare` used to make commits and push them.
pub struct Work {
    pub dir: tempfile::TempDir,
}

impl Work {
    pub fn clone_of(bare: &Path) -> Work {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["clone", "-q", bare.to_str().unwrap(), "."]);
        git(dir.path(), &["checkout", "-q", "-B", "main"]);
        Work { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write `file`, commit it and return the new commit id.
    pub fn commit(&self, file: &str, content: &str, message: &str) -> String {
        let path = self.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        git(self.path(), &["add", "-A"]);
        git(self.path(), &["commit", "-q", "-m", message]);
        git(self.path(), &["rev-parse", "HEAD"])
    }

    pub fn push(&self, refspec: &str) {
        git(self.path(), &["push", "-q", "origin", refspec]);
    }
}

/// Bare repo with one commit on `main` containing `files`.
pub fn repo_with(root: &Path, rel: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
    let bare = bare(root, rel, &[]);
    let work = Work::clone_of(&bare);
    let mut head = String::new();
    for (name, content) in files {
        head = work.commit(name, content, &format!("add {name}"));
    }
    work.push("main");
    (bare, head)
}

/// Ids of interesting objects in [`rich_repo`].
pub struct Rich {
    pub bare: PathBuf,
    pub head: String,
    pub first: String,
    pub feature: String,
}

/// A bare repo exercising the browsing edge cases: nested dirs, a README
/// with relative links, a binary and an oversize file, a unicode path, a
/// symlink, a submodule entry, a slashy branch and both kinds of tags.
pub fn rich_repo(root: &Path, rel: &str) -> Rich {
    let bare = bare(root, rel, &[]);
    let work = Work::clone_of(&bare);
    let w = work.path();
    let write = |name: &str, data: &[u8]| {
        let path = w.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    write(
        "src/main.rs",
        b"// entry\nfn main() {\n    println!(\"<hi>\");\n}\n",
    );
    let first = work.commit(
        "README.md",
        "# Rich\n\nSee [the guide](docs/guide.md) and ![logo](img/logo.png).\n\n\
         <script>alert(1)</script>\n",
        "first commit",
    );
    write("docs/guide.md", b"# Guide\n\n[Back](../README.md)\n");
    write("docs/README", b"Plain docs readme <b>not html</b>\n");
    write("page.html", b"<html><script>alert(1)</script></html>\n");
    write("img/logo.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    write("data.bin", b"\0\x01\x02binary\0");
    write("big.txt", &b"0123456789abcdef\n".repeat(200)); // 3400 bytes
    write("ünïcode dir/fïle #1.txt", b"unicode\n");
    std::os::unix::fs::symlink("src/main.rs", w.join("link-to-main")).unwrap();
    git(w, &["add", "-A"]);
    git(
        w,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},vendor/lib"),
        ],
    );
    git(w, &["commit", "-q", "-m", "add everything"]);
    let head = git(w, &["rev-parse", "HEAD"]);
    git(w, &["tag", "v1.0"]);
    git(
        w,
        &["tag", "-a", "v1.1", "-m", "Release 1.1", first.as_str()],
    );
    git(w, &["checkout", "-q", "-b", "feature/x"]);
    let feature = work.commit("only-on-feature.txt", "feature\n", "feature work");
    git(w, &["checkout", "-q", "main"]);
    git(w, &["push", "-q", "origin", "main", "feature/x", "--tags"]);
    Rich {
        bare,
        head,
        first,
        feature,
    }
}
