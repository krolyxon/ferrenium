//! `/{repo}/find/{rev}?q=` — find files by path.
//!
//! Every term must appear in the path (case-insensitively); matches in the
//! file name rank first, then shorter paths.

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;

use super::repo::{Active, Crumb, Ctx, RepoNav, set_cache_headers};
use super::{Layout, render};
use crate::git::tree::{self, PathTarget};

/// Paths indexed per tree; larger trees are searched partially.
const MAX_PATHS: usize = 200_000;
const MAX_RESULTS: usize = 100;

pub struct Hit {
    pub dir: String,
    pub name: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "find.html")]
pub struct FindPage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    pub action: String,
    pub q: String,
    pub hits: Vec<Hit>,
    pub matched: usize,
    pub total: usize,
    pub truncated: bool,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    if !path.is_empty() {
        return ctx.not_found("PAGE NOT FOUND");
    }
    let q = ctx
        .query
        .q
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    let etag = ctx.etag(("find", rev.commit, &q));
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let root = match tree::lookup(&ctx.git, rev.commit, b"") {
        Ok(Some(PathTarget::Tree(id))) => id,
        Ok(_) => return ctx.not_found("COMMIT HAS NO TREE"),
        Err(e) => return ctx.internal_error(e),
    };
    let paths = match ctx
        .state
        .cache
        .paths(root, || tree::all_paths(&ctx.git, root, MAX_PATHS))
    {
        Ok(paths) => paths,
        Err(e) => return ctx.internal_error(e),
    };
    let (all, truncated) = (&paths.0, paths.1);
    let ranked = search(all, &q);
    let hits = ranked
        .iter()
        .take(MAX_RESULTS)
        .map(|&i| {
            let path = &all[i];
            let (dir, name) = match path.rsplit_once('/') {
                Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                None => (String::new(), path.clone()),
            };
            Hit {
                dir,
                name,
                url: ctx.url("blob", &rev.name, path.as_bytes()),
            }
        })
        .collect();

    let page = FindPage {
        layout: Layout::new(
            ctx.state,
            ctx.config,
            Some(&format!("Find · {}", ctx.repo.name)),
        ),
        nav: ctx.nav(Active::Find, Some(&rev), |name| ctx.url("find", name, b"")),
        crumbs: vec![Crumb {
            name: ctx.repo.name.clone(),
            url: ctx.url("tree", &rev.name, b""),
        }],
        here: Some("Find".into()),
        action: ctx.url("find", &rev.name, b""),
        hits,
        matched: ranked.len(),
        total: all.len(),
        truncated,
        q,
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}

/// Indexes of the paths matching every term of `q`, best first.
pub fn search(paths: &[String], q: &str) -> Vec<usize> {
    let terms: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(bool, usize, usize)> = paths
        .iter()
        .enumerate()
        .filter_map(|(i, path)| {
            let lower = path.to_lowercase();
            if !terms.iter().all(|t| lower.contains(t.as_str())) {
                return None;
            }
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            let in_name = terms.iter().any(|t| name.contains(t.as_str()));
            Some((!in_name, path.len(), i))
        })
        .collect();
    hits.sort();
    hits.into_iter().map(|(_, _, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::search;

    #[test]
    fn ranking() {
        let paths: Vec<String> = [
            "src/main.rs",
            "main/readme.md",
            "src/very/deep/main.rs",
            "docs/Main.md",
            "src/lib.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let hits: Vec<&str> = search(&paths, "MAIN")
            .into_iter()
            .map(|i| paths[i].as_str())
            .collect();
        assert_eq!(
            hits,
            [
                "src/main.rs",
                "docs/Main.md",
                "src/very/deep/main.rs",
                "main/readme.md"
            ]
        );
        assert_eq!(search(&paths, "src rs").len(), 3);
        assert!(search(&paths, "   ").is_empty());
    }
}
