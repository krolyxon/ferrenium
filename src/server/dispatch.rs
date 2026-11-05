//! The catch-all route: map a URL path onto a repository and an action.
//!
//! Repository names may contain `/`, so the longest prefix of the path that
//! names a repository wins; the next segment is the action. Ferrenium's own
//! endpoints live under `/-/`, which never names a repository.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

use crate::handlers::error;
use crate::handlers::repo::{PageQuery, RepoRequest, handle};
use crate::index::{Repo, RepoIndex};
use crate::render::{encode_path, percent_decode};
use crate::state::AppState;

pub enum Match<'a> {
    Repo {
        repo: Arc<Repo>,
        rest: &'a [Vec<u8>],
    },
    /// `/{repo}.git` → `/{repo}`.
    DotGit {
        name: String,
    },
    None,
}

/// Split a raw URL path into percent-decoded segments, dropping empty ones.
/// `None` for `.`/`..` segments, which are never valid.
pub fn segments(path: &str) -> Option<Vec<Vec<u8>>> {
    let segments: Vec<Vec<u8>> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let bad = segments
        .iter()
        .any(|s| s == b"." || s == b".." || s.contains(&b'/') || s.contains(&0));
    (!bad).then_some(segments)
}

/// Find the repository named by the longest prefix of `segments`.
pub fn find_repo<'a>(index: &RepoIndex, segments: &'a [Vec<u8>]) -> Match<'a> {
    if segments.first().is_some_and(|s| s == b"-") {
        return Match::None;
    }
    for n in (1..=segments.len()).rev() {
        let parts: Option<Vec<&str>> = segments[..n]
            .iter()
            .map(|s| std::str::from_utf8(s).ok())
            .collect();
        let Some(parts) = parts else {
            continue;
        };
        let name = parts.join("/");
        if let Some(repo) = index.repos.get(&name) {
            return Match::Repo {
                repo: repo.clone(),
                rest: &segments[n..],
            };
        }
        if let Some(stripped) = name.strip_suffix(".git")
            && index.repos.contains_key(stripped)
        {
            // `/{repo}.git/…` is the HTTPS clone endpoint (not served yet).
            return if n == segments.len() {
                Match::DotGit {
                    name: stripped.to_string(),
                }
            } else {
                Match::None
            };
        }
    }
    Match::None
}

pub async fn dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let config = state.config.load_full();
    let index = state.index.load();
    let Some(segments) = segments(req.uri().path()) else {
        return error::page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND");
    };
    let (repo, rest) = match find_repo(&index, &segments) {
        Match::Repo { repo, rest } => (repo, rest.to_vec()),
        Match::DotGit { name } => {
            let url = format!("{}{}", config.server.base_path, encode_path(&name));
            return Redirect::permanent(&url).into_response();
        }
        Match::None => return error::page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND"),
    };
    let query = Query::<PageQuery>::try_from_uri(req.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    let request = RepoRequest {
        repo,
        segments: rest,
        query,
        if_none_match: req.headers().get(header::IF_NONE_MATCH).cloned(),
        config,
    };
    match state.blocking(move |state| handle(state, request)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!("request failed: {e:#}");
            error::page(&state, StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL ERROR")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RepoIndex;

    #[test]
    fn segment_parsing() {
        assert_eq!(
            segments("/a//b%2Fc/").map(|s| s.len()),
            None,
            "encoded slash is rejected"
        );
        assert_eq!(
            segments("/a//b/%C3%BC/").unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), "ü".as_bytes().to_vec()]
        );
        assert!(segments("/a/../b").is_none());
        assert!(segments("/a/%2e%2e/b").is_none());
        assert!(segments("/").unwrap().is_empty());
    }

    #[test]
    fn no_repos_no_match() {
        let index = RepoIndex::default();
        let segs = segments("/-/x").unwrap();
        assert!(matches!(find_repo(&index, &segs), Match::None));
    }
}
