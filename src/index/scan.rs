//! Filesystem discovery of repositories under `repos.scan_path`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A repository found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Path relative to the scan path, `.git` suffix stripped: `forks/foo`.
    pub name: String,
    /// The git dir: the bare repo itself, or `<worktree>/.git`.
    pub git_dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub candidates: Vec<Candidate>,
    /// Directories walked that are not repositories; watched for repos
    /// appearing or disappearing.
    pub dirs: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn discover(scan_path: &Path, max_depth: u32, follow_symlinks: bool) -> Discovery {
    let mut out = Discovery::default();
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();

    if repo_git_dir(scan_path).is_some() {
        out.warnings.push(format!(
            "repos.scan_path {} is itself a repository; point it at the directory containing repositories",
            scan_path.display()
        ));
        return out;
    }
    walk(
        scan_path,
        scan_path,
        0,
        max_depth,
        follow_symlinks,
        &mut found,
        &mut out,
    );
    out.candidates = found
        .into_iter()
        .map(|(name, git_dir)| Candidate { name, git_dir })
        .collect();
    out
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    follow_symlinks: bool,
    found: &mut BTreeMap<String, PathBuf>,
    out: &mut Discovery,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            out.warnings
                .push(format!("cannot read {}: {e}", dir.display()));
            return;
        }
    };
    out.dirs.push(dir.to_path_buf());

    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| !entry.file_name().as_encoded_bytes().starts_with(b"."))
        .filter(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() && follow_symlinks => entry.path().is_dir(),
            _ => false,
        })
        .map(|entry| entry.path())
        .collect();
    subdirs.sort();

    for path in subdirs {
        if let Some(git_dir) = repo_git_dir(&path) {
            let Some(name) = repo_name(root, &path) else {
                out.warnings.push(format!(
                    "skipping {}: path is not valid UTF-8",
                    path.display()
                ));
                continue;
            };
            if let Some(existing) = found.get(&name) {
                out.warnings.push(format!(
                    "name collision: {} and {} are both named {name:?}; keeping the first",
                    existing.display(),
                    git_dir.display()
                ));
                continue;
            }
            found.insert(name, git_dir);
        } else if depth + 1 < max_depth {
            walk(
                root,
                &path,
                depth + 1,
                max_depth,
                follow_symlinks,
                found,
                out,
            );
        }
    }
}

/// The git dir if `path` is a bare repository or a worktree with a `.git` dir.
pub fn repo_git_dir(path: &Path) -> Option<PathBuf> {
    if is_bare(path) {
        return Some(path.to_path_buf());
    }
    let dot_git = path.join(".git");
    is_bare(&dot_git).then_some(dot_git)
}

fn is_bare(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

fn repo_name(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?.to_str()?;
    let name = rel.strip_suffix(".git").unwrap_or(rel);
    (!name.is_empty() && !name.ends_with('/')).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bare(path: &Path) {
        std::fs::create_dir_all(path.join("objects")).unwrap();
        std::fs::create_dir_all(path.join("refs")).unwrap();
        std::fs::write(path.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn discovers_bare_and_worktree_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_bare(&root.join("a.git"));
        fake_bare(&root.join("forks/b.git"));
        fake_bare(&root.join("work/c/.git"));
        fake_bare(&root.join("deep/1/2/3/d.git")); // beyond max_depth 3
        fake_bare(&root.join(".hidden/e.git"));
        fake_bare(&root.join("a.git/nested.git")); // inside a repo: not walked
        std::fs::create_dir_all(root.join("empty")).unwrap();

        let d = discover(root, 3, false);
        let names: Vec<_> = d.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "forks/b", "work/c"]);
        assert_eq!(d.candidates[2].git_dir, root.join("work/c/.git"));
        assert!(d.dirs.contains(&root.join("forks")));
        assert!(d.dirs.contains(&root.join("empty")));
        assert!(!d.dirs.contains(&root.join("a.git")));
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn collisions_warn_and_keep_first() {
        let dir = tempfile::tempdir().unwrap();
        fake_bare(&dir.path().join("x"));
        fake_bare(&dir.path().join("x.git"));
        let d = discover(dir.path(), 3, false);
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(d.candidates[0].git_dir, dir.path().join("x"));
        assert!(d.warnings[0].contains("name collision"));
    }

    #[test]
    fn symlinks_followed_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fake_bare(&elsewhere.path().join("r.git"));
        std::os::unix::fs::symlink(elsewhere.path().join("r.git"), dir.path().join("r.git"))
            .unwrap();
        assert!(discover(dir.path(), 3, false).candidates.is_empty());
        assert_eq!(discover(dir.path(), 3, true).candidates.len(), 1);
    }
}
