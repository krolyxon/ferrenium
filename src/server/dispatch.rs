//! The catch-all route: map a URL path onto a repository and an action.
//!
//! Repository names may contain `/`, so the longest prefix of the path that
//! names a repository wins; the next segment is the action. Ferrenium's own
//! endpoints live under `/-/`, which never names a repository.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

use super::clone;
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
    /// `/{repo}.git` → `/{repo}`; `/{repo}.git/…` is only for cloning.
    DotGit {
        repo: Arc<Repo>,
        rest: &'a [Vec<u8>],
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
            && let Some(repo) = index.repos.get(stripped)
        {
            return Match::DotGit {
                repo: repo.clone(),
                rest: &segments[n..],
            };
        }
    }
    Match::None
}

/// Expensive pages share `limits.heavy_parallel` permits.
fn is_heavy(rest: &[Vec<u8>], query: &PageQuery) -> bool {
    match rest.first().map(Vec::as_slice) {
        Some(b"blame" | b"archive") => true,
        Some(b"log") => query.q.as_deref().is_some_and(|q| !q.trim().is_empty()),
        _ => false,
    }
}

pub async fn dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let config = state.config.load_full();
    let index = state.index.load();
    let Some(segments) = segments(req.uri().path()) else {
        return error::page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND");
    };
    let (repo, rest) = match find_repo(&index, &segments) {
        Match::Repo { repo, rest } => (repo, rest.to_vec()),
        Match::DotGit { repo, rest: [] } => {
            let url = format!("{}{}", config.server.base_path, encode_path(&repo.name));
            return Redirect::permanent(&url).into_response();
        }
        Match::DotGit { repo, rest } if clone::is_clone_path(rest) => {
            return clone::serve(state.clone(), repo, rest.to_vec(), req).await;
        }
        Match::DotGit { .. } | Match::None => {
            return error::page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND");
        }
    };
    if clone::is_clone_path(&rest) {
        return clone::serve(state.clone(), repo, rest, req).await;
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let query = Query::<PageQuery>::try_from_uri(req.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    let heavy = if is_heavy(&rest, &query) {
        match state.heavy_permits.clone().acquire_owned().await {
            Ok(permit) => Some(permit),
            Err(_) => return error::page(&state, StatusCode::SERVICE_UNAVAILABLE, "SHUTTING DOWN"),
        }
    } else {
        None
    };
    let request = RepoRequest {
        repo,
        heavy,
        host: req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string),
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
