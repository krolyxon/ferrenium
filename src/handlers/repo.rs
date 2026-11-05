//! Shared plumbing for repository pages: routing by action, revision
//! resolution, ETags and the repository nav.
//!
//! Everything here runs on the blocking pool (see [`AppState::blocking`]).

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::{blob, error, raw, refs, summary, tree};
use crate::cache::Key;
use crate::config::Config;
use crate::git::refs::{RefKind, RefList, Rev};
use crate::git::{self, Handle};
use crate::index::Repo;
use crate::render::{encode_bytes, encode_path};
use crate::state::AppState;

/// Tags listed in the ref picker; the rest are on the refs page.
const PICKER_TAGS: usize = 20;

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// Blob view: `source` shows Markdown as code.
    pub view: Option<String>,
    /// Raw: serve as an attachment.
    pub download: Option<String>,
}

pub struct RepoRequest {
    pub repo: Arc<Repo>,
    /// Path segments after the repository name.
    pub segments: Vec<Vec<u8>>,
    pub query: PageQuery,
    pub if_none_match: Option<HeaderValue>,
    pub config: Arc<Config>,
}

/// Everything a repository page handler needs.
pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub config: &'a Config,
    pub repo: &'a Repo,
    pub git: Handle,
    pub query: &'a PageQuery,
    if_none_match: Option<&'a HeaderValue>,
}

pub fn handle(state: &AppState, req: RepoRequest) -> Response {
    let ctx = Ctx {
        state,
        config: &req.config,
        repo: &req.repo,
        git: git::handle(&req.repo.gix),
        query: &req.query,
        if_none_match: req.if_none_match.as_ref(),
    };
    let Some((action, rest)) = req.segments.split_first() else {
        return summary::page(&ctx);
    };
    match action.as_slice() {
        b"tree" => tree::page(&ctx, rest),
        b"blob" => blob::page(&ctx, rest),
        b"raw" => raw::serve(&ctx, rest),
        b"refs" if rest.is_empty() => refs::page(&ctx),
        _ => ctx.not_found("PAGE NOT FOUND"),
    }
}

impl Ctx<'_> {
    /// `{base}{repo}` without a trailing slash.
    pub fn repo_url(&self) -> String {
        format!(
            "{}{}",
            self.config.server.base_path,
            encode_path(&self.repo.name)
        )
    }

    /// `{repo}/{action}/{rev}/{path}` (no trailing slash for the root).
    pub fn url(&self, action: &str, rev: &str, path: &[u8]) -> String {
        let mut url = format!("{}/{action}/{}", self.repo_url(), encode_path(rev));
        if !path.is_empty() {
            url.push('/');
            url.push_str(&encode_bytes(path));
        }
        url
    }

    pub fn not_found(&self, message: &str) -> Response {
        error::page(self.state, StatusCode::NOT_FOUND, message)
    }

    pub fn internal_error(&self, e: anyhow::Error) -> Response {
        tracing::error!("{}: {e:#}", self.repo.name);
        error::page(
            self.state,
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL ERROR",
        )
    }

    /// The revision and path named by `segments`; an empty list means the
    /// default branch at the root.
    pub fn resolve(&self, segments: &[Vec<u8>]) -> Result<(Rev, Vec<u8>), Box<Response>> {
        if segments.is_empty() {
            let (Some(branch), Some(head)) = (&self.repo.meta.default_branch, self.repo.meta.head)
            else {
                return Err(Box::new(self.not_found("REPOSITORY IS EMPTY")));
            };
            let rev = Rev {
                name: branch.clone(),
                kind: RefKind::Branch,
                commit: head,
            };
            return Ok((rev, Vec::new()));
        }
        match git::refs::resolve_prefix(&self.git, segments) {
            Some((rev, path)) => Ok((rev, path.join(&b'/'))),
            None => Err(Box::new(self.not_found("UNKNOWN REVISION"))),
        }
    }

    /// Weak ETag over everything a page depends on besides its URL: the
    /// content (`parts`), the repository's refs and metadata, the config and
    /// the build.
    pub fn etag(&self, parts: impl Hash) -> String {
        let mut hasher = std::hash::DefaultHasher::new();
        parts.hash(&mut hasher);
        self.repo.generation.hash(&mut hasher);
        self.state
            .config_generation
            .load(Ordering::Relaxed)
            .hash(&mut hasher);
        self.state.assets.hash().hash(&mut hasher);
        format!("W/\"{:016x}\"", hasher.finish())
    }

    /// `Some(304)` when the client already has `etag`.
    pub fn not_modified(&self, etag: &str, immutable: bool) -> Option<Response> {
        let header = self.if_none_match?.to_str().ok()?;
        let bare = |t: &str| t.trim().trim_start_matches("W/").to_string();
        let matches = header
            .split(',')
            .any(|t| t.trim() == "*" || bare(t) == bare(etag));
        matches.then(|| {
            let mut res = StatusCode::NOT_MODIFIED.into_response();
            set_cache_headers(&mut res, etag, immutable);
            res
        })
    }

    /// Branches and tags (with tip commits), cached per refs generation.
    pub fn refs(&self) -> anyhow::Result<Arc<RefList>> {
        let key = Key::Refs {
            repo: self.repo.name.clone(),
            generation: self.repo.generation,
        };
        self.state.cache.refs(key, || git::refs::list(&self.git))
    }

    /// Nav for this repository; `picker` builds ref-picker links for a
    /// revision name.
    pub fn nav(
        &self,
        active: Active,
        current: Option<&Rev>,
        picker: impl Fn(&str) -> String,
    ) -> RepoNav {
        let tree_url = match current {
            Some(rev) => self.url("tree", &rev.name, b""),
            None => format!("{}/tree", self.repo_url()),
        };
        let picker = current.and_then(|rev| {
            let refs = self.refs().ok()?;
            let item = |name: &str| PickItem {
                name: name.to_string(),
                url: picker(name),
                active: rev.kind != RefKind::Commit && rev.name == name,
            };
            Some(Picker {
                current: match rev.kind {
                    RefKind::Commit => short(&rev.commit.to_string()),
                    _ => rev.name.clone(),
                },
                kind: match rev.kind {
                    RefKind::Branch => "Branch",
                    RefKind::Tag => "Tag",
                    RefKind::Commit => "Commit",
                },
                branches: refs.branches.iter().map(|r| item(&r.name)).collect(),
                tags: refs
                    .tags
                    .iter()
                    .take(PICKER_TAGS)
                    .map(|r| item(&r.name))
                    .collect(),
                more_tags: refs.tags.len() > PICKER_TAGS,
            })
        });
        RepoNav {
            name: self.repo.name.clone(),
            url: self.repo_url(),
            tree_url,
            refs_url: format!("{}/refs", self.repo_url()),
            active: active.as_str(),
            picker,
        }
    }
}

/// Set `ETag` and `Cache-Control` for a repository page.
pub fn set_cache_headers(res: &mut Response, etag: &str, immutable: bool) {
    if let Ok(value) = HeaderValue::from_str(etag) {
        res.headers_mut().insert(header::ETAG, value);
    }
    let cache = if immutable {
        "public, max-age=86400"
    } else {
        "no-cache"
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
}

/// Redirect between the tree and blob views of the same path.
pub fn redirect(url: &str) -> Response {
    Redirect::temporary(url).into_response()
}

pub fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Active {
    Summary,
    Tree,
    Refs,
}

impl Active {
    pub fn as_str(self) -> &'static str {
        match self {
            Active::Summary => "summary",
            Active::Tree => "tree",
            Active::Refs => "refs",
        }
    }
}

pub struct RepoNav {
    pub name: String,
    pub url: String,
    pub tree_url: String,
    pub refs_url: String,
    /// `summary`, `tree` or `refs`.
    pub active: &'static str,
    pub picker: Option<Picker>,
}

pub struct Picker {
    /// Label of the current revision (short id for commits).
    pub current: String,
    pub kind: &'static str,
    pub branches: Vec<PickItem>,
    pub tags: Vec<PickItem>,
    pub more_tags: bool,
}

pub struct PickItem {
    pub name: String,
    pub url: String,
    pub active: bool,
}

/// One breadcrumb segment: the repository name, then each directory.
pub struct Crumb {
    pub name: String,
    pub url: String,
}

/// Breadcrumbs for `path` at `rev`; the last segment is returned separately
/// (it is the current page and not a link).
pub fn crumbs(ctx: &Ctx, rev: &str, path: &[u8]) -> (Vec<Crumb>, Option<String>) {
    let mut crumbs = vec![Crumb {
        name: ctx.repo.name.clone(),
        url: ctx.url("tree", rev, b""),
    }];
    if path.is_empty() {
        return (crumbs, None);
    }
    let parts: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
    let (last, dirs) = parts.split_last().expect("non-empty path");
    for (i, part) in dirs.iter().enumerate() {
        crumbs.push(Crumb {
            name: String::from_utf8_lossy(part).into_owned(),
            url: ctx.url("tree", rev, &parts[..=i].join(&b'/')),
        });
    }
    (crumbs, Some(String::from_utf8_lossy(last).into_owned()))
}
