//! `/{repo}/atom` — Atom feed of the newest commits on the default branch
//! (`?h=branch` for another one).

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

use super::render;
use super::repo::{Ctx, set_cache_headers};
use crate::cache::Key;
use crate::git::log::{self, LogQuery};
use crate::git::refs::{RefKind, Rev};
use crate::git::{self, refs};
use crate::render::time;

const ENTRIES: usize = 20;

pub struct Entry {
    pub title: String,
    pub url: String,
    pub updated: String,
    pub published: String,
    pub author: String,
    pub email: String,
    pub message: String,
}

#[derive(Template)]
#[template(path = "atom.xml")]
pub struct Feed {
    pub title: String,
    pub subtitle: Option<String>,
    pub self_url: String,
    pub html_url: String,
    pub updated: String,
    pub version: &'static str,
    pub entries: Vec<Entry>,
}

pub fn serve(ctx: &Ctx) -> Response {
    let rev = match ctx.query.h.as_deref().filter(|h| !h.is_empty()) {
        Some(branch) => match refs::resolve(&ctx.git, branch) {
            Some(rev) if rev.kind == RefKind::Branch => rev,
            _ => return ctx.not_found("UNKNOWN BRANCH"),
        },
        None => {
            let meta = &ctx.repo.meta;
            match (&meta.default_branch, meta.head) {
                (Some(name), Some(commit)) => Rev {
                    name: name.clone(),
                    kind: RefKind::Branch,
                    commit,
                },
                _ => return ctx.not_found("REPOSITORY IS EMPTY"),
            }
        }
    };
    let etag = ctx.etag(("atom", rev.commit));
    if let Some(res) = ctx.not_modified(&etag, false) {
        return res;
    }
    let query = LogQuery {
        page: ENTRIES,
        scan_limit: ctx.config.limits.log_scan,
        ..Default::default()
    };
    let key = Key::Log {
        tip: rev.commit,
        query: query.clone(),
    };
    let page = match ctx
        .state
        .cache
        .log(key, || log::page(&ctx.git, rev.commit, &query))
    {
        Ok(page) => page,
        Err(e) => return ctx.internal_error(anyhow::anyhow!("{e}")),
    };

    let origin = ctx.origin();
    let mailmap = ctx.mailmap();
    let mut entries = Vec::with_capacity(page.commits.len());
    for commit in &page.commits {
        let detail = match git::commit_detail(&ctx.git, commit.id) {
            Ok(detail) => detail,
            Err(e) => return ctx.internal_error(e),
        };
        let (author, email) = mailmap.resolve(&detail.author.name, &detail.author.email);
        entries.push(Entry {
            title: commit.subject.clone(),
            url: format!("{origin}{}", ctx.commit_url(&commit.id)),
            updated: time::iso(detail.committer.time),
            published: time::iso(detail.author.time),
            author,
            email,
            message: detail.message.trim_end().to_string(),
        });
    }
    let self_url = match ctx.query.h.as_deref().filter(|h| !h.is_empty()) {
        Some(_) => format!(
            "{origin}{}/atom?h={}",
            ctx.repo_url(),
            crate::render::encode_path(&rev.name)
        ),
        None => format!("{origin}{}/atom", ctx.repo_url()),
    };
    let feed = Feed {
        title: format!("{}: {}", ctx.repo.name, rev.name),
        subtitle: ctx.repo.meta.description.clone(),
        html_url: format!("{origin}{}", ctx.url("log", &rev.name, b"")),
        self_url,
        updated: page
            .commits
            .first()
            .map(|c| time::iso(c.committer_time))
            .unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION"),
        entries,
    };
    let mut res = render(StatusCode::OK, &feed);
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/atom+xml; charset=utf-8"),
    );
    set_cache_headers(&mut res, &etag, false);
    res
}
