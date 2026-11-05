//! `/{repo}/refs` — branches and tags.

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;

use super::repo::{Active, Crumb, Ctx, RepoNav, set_cache_headers};
use super::{Layout, render};
use crate::config::SnapshotFormat;
use crate::git::refs::RefEntry;
use crate::render::encode_path;
use crate::render::time;

pub struct RefRow {
    pub name: String,
    pub url: String,
    pub subject: String,
    pub age: String,
    pub iso: String,
    pub is_default: bool,
    /// Snapshot downloads (`tar.gz`, `zip`), on tags.
    pub archives: Vec<(&'static str, String)>,
}

#[derive(Template)]
#[template(path = "refs.html")]
pub struct RefsPage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    pub branches: Vec<RefRow>,
    pub tags: Vec<RefRow>,
}

pub fn page(ctx: &Ctx) -> Response {
    let etag = ctx.etag("refs");
    if let Some(res) = ctx.not_modified(&etag, false) {
        return res;
    }
    let refs = match ctx.refs() {
        Ok(refs) => refs,
        Err(e) => return ctx.internal_error(e),
    };
    let now = time::now();
    let default = ctx.repo.meta.default_branch.as_deref();
    let row = |r: &RefEntry| RefRow {
        name: r.name.clone(),
        url: ctx.url("tree", &r.name, b""),
        subject: r
            .tag_subject
            .clone()
            .unwrap_or_else(|| r.info.subject.clone()),
        age: time::relative(r.info.committer_time, now),
        iso: time::iso(r.info.committer_time),
        is_default: false,
        archives: Vec::new(),
    };
    let mut branches: Vec<RefRow> = refs.branches.iter().map(row).collect();
    // The default branch leads the list.
    if let Some(i) = branches
        .iter()
        .position(|b| Some(b.name.as_str()) == default)
    {
        let mut b = branches.remove(i);
        b.is_default = true;
        branches.insert(0, b);
    }
    let crumbs = vec![Crumb {
        name: ctx.repo.name.clone(),
        url: ctx.repo_url(),
    }];
    let page = RefsPage {
        layout: Layout::new(
            ctx.state,
            ctx.config,
            Some(&format!("Refs · {}", ctx.repo.name)),
        ),
        nav: ctx.nav(Active::Refs, None, |_| String::new()),
        crumbs,
        here: Some("Refs".into()),
        branches,
        tags: refs
            .tags
            .iter()
            .map(|r| {
                let mut row = row(r);
                row.archives = archives(ctx, &r.name);
                row
            })
            .collect(),
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, false);
    res
}

/// Links to the enabled snapshot formats of `rev`.
fn archives(ctx: &Ctx, rev: &str) -> Vec<(&'static str, String)> {
    ctx.config
        .repos
        .snapshots
        .iter()
        .map(|format| {
            let ext = match format {
                SnapshotFormat::TarGz => "tar.gz",
                SnapshotFormat::Zip => "zip",
            };
            let url = format!("{}/archive/{}.{ext}", ctx.repo_url(), encode_path(rev));
            (ext, url)
        })
        .collect()
}
