//! Per-repository metadata, resolved by precedence:
//! `[[repo]]` in ferrenium.toml > repo git config > `description` file >
//! defaults.

use std::path::Path;

use gix::bstr::BStr;
use gix::glob::wildmatch;

use super::scan::Candidate;
use crate::config::{Config, RepoOverride};
use crate::git::{self, ObjectId, ThreadSafeRepository};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readme {
    pub path: String,
    pub oid: ObjectId,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepoMeta {
    pub description: Option<String>,
    pub owner: Option<String>,
    pub section: Option<String>,
    /// Branch shown by default (may be unborn in an empty repo).
    pub default_branch: Option<String>,
    /// Commit the default branch points to; `None` for an empty repo.
    pub head: Option<ObjectId>,
    pub branches: usize,
    pub tags: usize,
    /// Unix seconds of the newest commit across branch tips.
    pub last_activity: Option<i64>,
    pub readme: Option<Readme>,
    pub refs_fingerprint: u64,
}

pub struct Inspected {
    pub gix: ThreadSafeRepository,
    pub meta: RepoMeta,
    pub hidden: bool,
    pub warnings: Vec<String>,
}

/// git's placeholder `description`, which means "no description".
const PLACEHOLDER_DESCRIPTION: &str = "Unnamed repository;";

pub fn inspect(config: &Config, candidate: &Candidate) -> anyhow::Result<Inspected> {
    let gix = git::open(&candidate.git_dir)?;
    let repo = gix.to_thread_local();
    let name = candidate.name.as_str();
    let toml = config.repo_overrides.iter().find(|r| r.name == name);
    let toml = toml.cloned().unwrap_or_default();
    let cfg = git::config_meta(&repo);
    let mut warnings = Vec::new();

    let hidden =
        is_hidden_by_glob(&config.repos.hide, name) || toml.hidden.or(cfg.hidden).unwrap_or(false);
    if hidden {
        return Ok(Inspected {
            gix,
            meta: RepoMeta::default(),
            hidden,
            warnings,
        });
    }

    let section = toml.section.clone().or(cfg.section.clone()).or_else(|| {
        config
            .repos
            .section_from_path
            .then(|| name.split_once('/').map(|(first, _)| first.to_string()))
            .flatten()
    });

    let head_branch = git::head_branch(&repo);
    let mut default_branch = toml
        .default_branch
        .clone()
        .or(cfg.default_branch.clone())
        .or(head_branch.clone());
    let mut head = default_branch
        .as_deref()
        .and_then(|b| git::branch_commit(&repo, b));
    if head.is_none()
        && let Some(fallback) = head_branch.as_deref()
        && default_branch.as_deref() != Some(fallback)
        && let Some(commit) = git::branch_commit(&repo, fallback)
    {
        warnings.push(format!(
            "{name}: default branch {:?} does not exist; using {fallback:?}",
            default_branch.as_deref().unwrap_or_default()
        ));
        default_branch = Some(fallback.to_string());
        head = Some(commit);
    }

    let readme_path = toml.readme.clone().or(cfg.readme.clone());
    let readme = head.and_then(|commit| {
        let found = git::find_readme(&repo, commit, readme_path.as_deref());
        if found.is_none()
            && let Some(path) = &readme_path
        {
            warnings.push(format!("{name}: readme {path:?} not found"));
        }
        found.map(|(path, oid)| Readme { path, oid })
    });

    let refs = git::ref_summary(&repo)?;
    let meta = RepoMeta {
        description: description(&toml, cfg.description.clone(), &candidate.git_dir),
        owner: toml.owner.clone().or(cfg.owner.clone()),
        section,
        default_branch,
        head,
        branches: refs.branches,
        tags: refs.tags,
        last_activity: refs.last_activity,
        readme,
        refs_fingerprint: refs.fingerprint,
    };
    Ok(Inspected {
        gix,
        meta,
        hidden,
        warnings,
    })
}

fn description(toml: &RepoOverride, from_config: Option<String>, git_dir: &Path) -> Option<String> {
    toml.description
        .clone()
        .or(from_config)
        .or_else(|| {
            let text = std::fs::read_to_string(git_dir.join("description")).ok()?;
            let text = text.trim();
            (!text.is_empty() && !text.starts_with(PLACEHOLDER_DESCRIPTION))
                .then(|| text.to_string())
        })
        .filter(|d| !d.trim().is_empty())
}

/// `repos.hide` globs: `*` stays within one path component, `**` crosses.
pub fn is_hidden_by_glob(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| {
        wildmatch(
            BStr::new(pattern.as_bytes()),
            BStr::new(name.as_bytes()),
            wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_globs() {
        let pats = vec!["private/*".to_string(), "*-wip".to_string()];
        assert!(is_hidden_by_glob(&pats, "private/x"));
        assert!(!is_hidden_by_glob(&pats, "private/x/y"));
        assert!(is_hidden_by_glob(&pats, "thing-wip"));
        assert!(!is_hidden_by_glob(&pats, "dir/thing-wip"));
        assert!(!is_hidden_by_glob(&pats, "public"));
        assert!(is_hidden_by_glob(&["**/secret".into()], "a/b/secret"));
    }
}
