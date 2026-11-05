//! `/{repo}/log/{rev}[/{path…}]` — history, newest first, grouped by day.
//! `?after=<oid>` pages, `?q=` filters by message and author.

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;
use gix::bstr::ByteSlice;

use super::repo::{Active, Crumb, Ctx, RepoNav, crumbs, set_cache_headers, short};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::ObjectId;
use crate::git::log::{self, LogError, LogQuery, Newer};
use crate::render::{encode_path, time};

pub struct LogRow {
    pub subject: String,
    pub url: String,
    pub author: String,
    pub age: String,
    pub iso: String,
    pub id: String,
}

pub struct Day {
    pub label: String,
    pub rows: Vec<LogRow>,
}

#[derive(Template)]
#[template(path = "log.html")]
pub struct LogPage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    pub days: Vec<Day>,
    /// The form posts here (the log URL without query).
    pub action: String,
    pub q: String,
    pub newer_url: Option<String>,
    /// `Newer`, or `Newest` when the link jumps back to the first page.
    pub newer_label: &'static str,
    pub older_url: Option<String>,
    pub limit_reached: bool,
    pub scan_limit: usize,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let after = match ctx.query.after.as_deref().filter(|a| !a.is_empty()) {
        None => None,
        Some(hex) => match ObjectId::from_hex(hex.as_bytes()) {
            Ok(id) => Some(id),
            Err(_) => return ctx.not_found("INVALID CURSOR"),
        },
    };
    let grep = ctx
        .query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_string);
    let query = LogQuery {
        path: path.clone(),
        grep,
        after,
        page: ctx.config.ui.log_page,
        scan_limit: ctx.config.limits.log_scan,
    };
    let etag = ctx.etag(("log", rev.commit, &query));
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let key = Key::Log {
        tip: rev.commit,
        query: query.clone(),
    };
    let result = ctx
        .state
        .cache
        .log(key, || log::page(&ctx.git, rev.commit, &query));
    let found = match result {
        Ok(found) => found,
        Err(LogError::UnknownCursor) => return ctx.not_found("COMMIT NOT IN THIS HISTORY"),
        Err(LogError::Git(e)) => return ctx.internal_error(e),
    };

    let action = ctx.url("log", &rev.name, &path);
    let link = |after: Option<ObjectId>| {
        let mut params = Vec::new();
        if let Some(q) = &query.grep {
            params.push(format!("q={}", encode_path(q)));
        }
        if let Some(id) = after {
            params.push(format!("after={id}"));
        }
        if params.is_empty() {
            action.clone()
        } else {
            format!("{action}?{}", params.join("&"))
        }
    };
    let (newer_url, newer_label) = match found.newer {
        Newer::None => (None, "Newer"),
        Newer::First => (Some(link(None)), "Newest"),
        Newer::After(id) => (Some(link(Some(id))), "Newer"),
    };

    let mailmap = ctx.mailmap();
    let now = time::now();
    let mut days: Vec<Day> = Vec::new();
    for commit in &found.commits {
        let label = time::day(commit.committer_time);
        let id = commit.id.to_string();
        let row = LogRow {
            subject: if commit.subject.is_empty() {
                "(no message)".into()
            } else {
                commit.subject.clone()
            },
            url: ctx.commit_url(&commit.id),
            author: mailmap.resolve(&commit.author_name, &commit.author_email).0,
            age: time::relative(commit.committer_time, now),
            iso: time::iso(commit.committer_time),
            id: short(&id),
        };
        match days.last_mut() {
            Some(day) if day.label == label => day.rows.push(row),
            _ => days.push(Day {
                label,
                rows: vec![row],
            }),
        }
    }

    let (crumbs, last) = crumbs(ctx, &rev.name, &path);
    let title = if path.is_empty() {
        format!("Log · {}", ctx.repo.name)
    } else {
        format!("History of {} · {}", path.to_str_lossy(), ctx.repo.name)
    };
    let older_url = found.older.map(|id| link(Some(id)));
    let page = LogPage {
        layout: Layout {
            feed: Some(ctx.feed_url()),
            ..Layout::new(ctx.state, ctx.config, Some(&title))
        },
        nav: ctx.nav(Active::Log, Some(&rev), |name| ctx.url("log", name, &path)),
        crumbs,
        here: last,
        days,
        action,
        q: query.grep.clone().unwrap_or_default(),
        newer_url,
        newer_label,
        older_url,
        limit_reached: found.limit_reached,
        scan_limit: query.scan_limit,
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}
