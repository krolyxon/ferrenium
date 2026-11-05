//! `/{repo}/blame/{rev}/{path…}` — each line with the commit that last
//! changed it. Hunks carry an age bar: newest at full strength, oldest
//! faint.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;
use gix::bstr::ByteSlice;

use super::repo::{Active, Crumb, Ctx, RepoNav, crumbs, redirect, set_cache_headers, short};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::tree::{self, PathTarget};
use crate::git::{self, CommitInfo, ObjectId};
use crate::render::{highlight, human_size, time};

/// Opacity steps of the age bar (`age-0` oldest … `age-8` newest).
const AGE_STEPS: i64 = 8;

pub struct HunkHead {
    pub id: String,
    pub url: String,
    pub subject: String,
    pub author: String,
    pub age: String,
    pub iso: String,
    pub age_class: String,
    pub rows: u32,
}

pub struct BlameRow {
    pub n: u32,
    pub html: String,
    /// Set on the first line of each hunk.
    pub head: Option<HunkHead>,
}

pub enum Body {
    Lines(Vec<BlameRow>),
    Binary,
    TooLarge,
    Empty,
}

#[derive(Template)]
#[template(path = "blame.html")]
pub struct BlamePage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    pub facts: Vec<String>,
    pub blob_url: String,
    pub log_url: String,
    pub body: Body,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let oid = match tree::lookup(&ctx.git, rev.commit, &path) {
        Ok(Some(PathTarget::Blob(oid, _))) => oid,
        Ok(Some(PathTarget::Tree(_))) => return redirect(&ctx.url("tree", &rev.name, &path)),
        Ok(Some(PathTarget::Submodule(_))) => return ctx.not_found("PATH IS A SUBMODULE"),
        Ok(None) => return ctx.not_found(&format!("PATH NOT FOUND IN {}", rev.name)),
        Err(e) => return ctx.internal_error(e),
    };
    let etag = ctx.etag(("blame", rev.commit, &path));
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let size = match tree::size(&ctx.git, oid) {
        Ok(size) => size,
        Err(e) => return ctx.internal_error(e),
    };
    let path_str = path.to_str_lossy().into_owned();
    let mut facts = vec![human_size(size)];

    let body = if size == 0 {
        Body::Empty
    } else if size > ctx.config.ui.max_blob_kb * 1024 {
        Body::TooLarge
    } else {
        let data = match tree::blob(&ctx.git, oid) {
            Ok(data) => data,
            Err(e) => return ctx.internal_error(e),
        };
        if data[..data.len().min(8000)].contains(&0) {
            Body::Binary
        } else {
            match lines(ctx, rev.commit, &path, oid, &path_str, &data) {
                Ok((rows, commits)) => {
                    facts.insert(0, format!("{commits} {}", plural(commits, "commit")));
                    facts.insert(0, format!("{} {}", rows.len(), plural(rows.len(), "line")));
                    Body::Lines(rows)
                }
                Err(e) => return ctx.internal_error(e),
            }
        }
    };

    let (crumbs, here) = crumbs(ctx, &rev.name, &path);
    let page = BlamePage {
        layout: Layout::new(
            ctx.state,
            ctx.config,
            Some(&format!("Blame {path_str} · {}", ctx.repo.name)),
        ),
        nav: ctx.nav(Active::Tree, Some(&rev), |name| {
            ctx.url("blame", name, &path)
        }),
        crumbs,
        here,
        facts,
        blob_url: ctx.url("blob", &rev.name, &path),
        log_url: ctx.url("log", &rev.name, &path),
        body,
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Rows for every line, plus the number of distinct commits.
fn lines(
    ctx: &Ctx,
    commit: ObjectId,
    path: &[u8],
    oid: ObjectId,
    path_str: &str,
    data: &[u8],
) -> anyhow::Result<(Vec<BlameRow>, usize)> {
    let key = Key::Blame {
        commit,
        path: path.to_vec(),
    };
    let hunks = ctx
        .state
        .cache
        .blame(key, || git::blame::blame(&ctx.git, commit, path))?;
    let text = String::from_utf8_lossy(data);
    let (html, _) = highlight::cached_lines(
        &ctx.state.cache,
        oid,
        path_str,
        &text,
        ctx.config.ui.max_highlight_kb * 1024,
    );

    let mut infos: HashMap<ObjectId, CommitInfo> = HashMap::new();
    for hunk in hunks.iter() {
        if let std::collections::hash_map::Entry::Vacant(slot) = infos.entry(hunk.commit) {
            slot.insert(git::commit_info(&ctx.git, hunk.commit)?);
        }
    }
    let times = infos.values().map(|i| i.committer_time);
    let (oldest, newest) = (times.clone().min().unwrap_or(0), times.max().unwrap_or(0));
    let mailmap = ctx.mailmap();
    let now = time::now();

    let mut heads: HashMap<u32, HunkHead> = HashMap::new();
    for hunk in hunks.iter() {
        let info = &infos[&hunk.commit];
        let step = if newest > oldest {
            (info.committer_time - oldest) * AGE_STEPS / (newest - oldest)
        } else {
            AGE_STEPS
        };
        let id = info.id.to_string();
        heads.insert(
            hunk.start,
            HunkHead {
                url: ctx.commit_url(&info.id),
                id: short(&id),
                subject: info.subject.clone(),
                author: mailmap.resolve(&info.author_name, &info.author_email).0,
                age: time::relative(info.committer_time, now),
                iso: time::iso(info.committer_time),
                age_class: format!("age-{step}"),
                rows: hunk.len,
            },
        );
    }
    let rows = rows(&html, heads);
    Ok((rows, infos.len()))
}

fn rows(html: &Arc<Vec<String>>, mut heads: HashMap<u32, HunkHead>) -> Vec<BlameRow> {
    html.iter()
        .enumerate()
        .map(|(i, line)| BlameRow {
            n: i as u32 + 1,
            html: line.clone(),
            head: heads.remove(&(i as u32)),
        })
        .collect()
}
