//! `/{repo}/commit/{rev}` — message, metadata, diffstat and the unified
//! diff against the first parent (`?context=N`, `?ws=ignore`).
//! `/{repo}/commit/{rev}.patch` — the same as a plain-text patch.

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use gix::bstr::ByteSlice;

use super::raw::RAW_CSP;
use super::repo::{Active, Crumb, Ctx, RepoNav, set_cache_headers, short};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::diff::Status;
use crate::git::refs::{self, Rev};
use crate::git::{self, CommitDetail};
use crate::render::diff::{self, Body, CommitDiff, FileDiff};
use crate::render::time;

const DEFAULT_CONTEXT: u32 = 3;
const MAX_CONTEXT: u32 = 50;

pub struct Meta {
    pub label: &'static str,
    pub value: String,
    pub title: Option<String>,
    pub links: Vec<(String, String)>,
}

pub struct FileRow<'a> {
    pub file: &'a FileDiff,
    pub anchor: String,
    pub path: String,
    /// Source path of a rename or copy.
    pub from: Option<String>,
    pub status: &'static str,
    /// Single-letter status (`A`, `M`, `D`, `R`, `C`).
    pub letter: &'static str,
    /// Five blocks: `add`, `del` or `none`.
    pub bar: [&'static str; 5],
    /// The file at this commit (or its parent, for deletions).
    pub url: Option<String>,
}

pub struct Toggle {
    pub label: &'static str,
    pub url: String,
    pub active: bool,
}

#[derive(Template)]
#[template(path = "commit.html")]
pub struct CommitPage<'a> {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    pub subject: String,
    pub body: String,
    pub meta: Vec<Meta>,
    pub added: u64,
    pub removed: u64,
    pub files: Vec<FileRow<'a>>,
    pub diff: &'a CommitDiff,
    pub patch_url: String,
    pub tree_url: String,
    pub ws_toggle: Vec<Toggle>,
    pub context_less: Option<String>,
    pub context_more: Option<String>,
    pub context: u32,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let Some((rev, patch)) = resolve(ctx, segments) else {
        return ctx.not_found("UNKNOWN REVISION");
    };
    let context = ctx
        .query
        .context
        .as_deref()
        .and_then(|c| c.parse::<u32>().ok())
        .map_or(DEFAULT_CONTEXT, |c| c.min(MAX_CONTEXT));
    let ignore_ws = ctx.query.ws.as_deref() == Some("ignore");
    let etag = if patch {
        ctx.etag(("patch", rev.commit))
    } else {
        ctx.etag(("commit", rev.commit, context, ignore_ws))
    };
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let detail = match git::commit_detail(&ctx.git, rev.commit) {
        Ok(detail) => detail,
        Err(e) => return ctx.internal_error(e),
    };
    let changes = match git::diff::changes(&ctx.git, rev.commit) {
        Ok(changes) => changes,
        Err(e) => return ctx.internal_error(e),
    };
    if patch {
        return match diff::patch(&ctx.git, &detail, &changes) {
            Ok(text) => {
                let mut res = text.into_response();
                let headers = res.headers_mut();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                headers.insert(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(RAW_CSP),
                );
                headers.insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                set_cache_headers(&mut res, &etag, immutable);
                res
            }
            Err(e) => ctx.internal_error(e),
        };
    }

    let options = diff::Options {
        context,
        ignore_ws,
        max_files: ctx.config.limits.diff_files,
        max_lines: ctx.config.limits.diff_lines,
        max_file_lines: ctx.config.limits.diff_file_lines,
        max_blob_bytes: ctx.config.ui.max_blob_kb * 1024,
        max_highlight_bytes: ctx.config.ui.max_highlight_kb * 1024,
    };
    let key = Key::Diff {
        commit: rev.commit,
        options,
    };
    let built = ctx.state.cache.diff(key, || {
        diff::build(&ctx.git, &ctx.state.cache, &changes, &options)
    });
    let built = match built {
        Ok(built) => built,
        Err(e) => return ctx.internal_error(e),
    };

    let mut page = commit_page(ctx, &rev, &detail, &built, changes.parent);
    page.context = context;
    let base = ctx.commit_url(&rev.commit);
    let url = |context: u32, ignore_ws: bool| {
        let mut params = Vec::new();
        if context != DEFAULT_CONTEXT {
            params.push(format!("context={context}"));
        }
        if ignore_ws {
            params.push("ws=ignore".to_string());
        }
        if params.is_empty() {
            base.clone()
        } else {
            format!("{base}?{}", params.join("&"))
        }
    };
    page.ws_toggle = vec![
        Toggle {
            label: "Whitespace",
            url: url(context, false),
            active: !ignore_ws,
        },
        Toggle {
            label: "Ignore",
            url: url(context, true),
            active: ignore_ws,
        },
    ];
    page.context_less = (context > 0).then(|| url(context.saturating_sub(3), ignore_ws));
    page.context_more =
        (context < MAX_CONTEXT).then(|| url((context + 3).min(MAX_CONTEXT), ignore_ws));

    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}

/// The revision and whether a patch was asked for. `{rev}.patch` is a
/// patch unless a ref with that literal name exists.
fn resolve(ctx: &Ctx, segments: &[Vec<u8>]) -> Option<(Rev, bool)> {
    let name = std::str::from_utf8(&segments.join(&b'/')).ok()?.to_string();
    if name.is_empty() {
        return None;
    }
    if let Some(stem) = name.strip_suffix(".patch")
        && let Some(rev) = refs::resolve(&ctx.git, stem)
        && refs::resolve(&ctx.git, &name).is_none()
    {
        return Some((rev, true));
    }
    refs::resolve(&ctx.git, &name).map(|rev| (rev, false))
}

fn commit_page<'a>(
    ctx: &Ctx,
    rev: &Rev,
    detail: &CommitDetail,
    built: &'a CommitDiff,
    parent: Option<git::ObjectId>,
) -> CommitPage<'a> {
    let mailmap = ctx.mailmap();
    let id = detail.info.id.to_string();
    let (author, author_email) = mailmap.resolve(&detail.author.name, &detail.author.email);
    let (committer, committer_email) =
        mailmap.resolve(&detail.committer.name, &detail.committer.email);

    let mut meta = vec![Meta {
        label: "Author",
        value: author.clone(),
        title: Some(author_email.clone()),
        links: Vec::new(),
    }];
    meta.push(Meta {
        label: "Date",
        value: time::local(detail.author.time, detail.author.offset),
        title: Some(time::iso(detail.author.time)),
        links: Vec::new(),
    });
    if (&committer, &committer_email) != (&author, &author_email)
        || detail.committer.time != detail.author.time
    {
        meta.push(Meta {
            label: "Committer",
            value: format!(
                "{committer} · {}",
                time::local(detail.committer.time, detail.committer.offset)
            ),
            title: Some(committer_email),
            links: Vec::new(),
        });
    }
    meta.push(Meta {
        label: if detail.parents.len() == 1 {
            "Parent"
        } else {
            "Parents"
        },
        value: if detail.parents.is_empty() {
            "None".into()
        } else {
            String::new()
        },
        title: None,
        links: detail
            .parents
            .iter()
            .map(|p| (short(&p.to_string()), ctx.commit_url(p)))
            .collect(),
    });
    meta.push(Meta {
        label: "Commit",
        value: id.clone(),
        title: None,
        links: Vec::new(),
    });

    let tree_url = ctx.url("tree", &id, b"");
    let parent_id = parent.map(|p| p.to_string());
    let files = built
        .files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let change = &file.change;
            let path = change.path().to_str_lossy().into_owned();
            let url = match change.status {
                Status::Deleted => parent_id
                    .as_ref()
                    .map(|p| ctx.url("blob", p, change.path())),
                _ => Some(ctx.url("blob", &id, change.path())),
            };
            let from = match change.status {
                Status::Renamed | Status::Copied => change
                    .old
                    .as_ref()
                    .map(|o| o.path.to_str_lossy().into_owned()),
                _ => None,
            };
            let (status, letter) = match change.status {
                Status::Added => ("Added", "A"),
                Status::Deleted => ("Deleted", "D"),
                Status::Modified => ("Modified", "M"),
                Status::Renamed => ("Renamed", "R"),
                Status::Copied => ("Copied", "C"),
            };
            // Submodules and binaries have no line counts but still have
            // a URL; only blobs get one.
            let url = url.filter(|_| !matches!(file.body, Body::Submodule { .. }));
            FileRow {
                file,
                anchor: format!("f{}", i + 1),
                path,
                from,
                status,
                letter,
                bar: bar(file.stat),
                url,
            }
        })
        .collect();

    CommitPage {
        layout: Layout::new(
            ctx.state,
            ctx.config,
            Some(&format!("{} · {}", detail.info.subject, ctx.repo.name)),
        ),
        nav: ctx.nav(Active::Log, Some(rev), |name| ctx.url("log", name, b"")),
        crumbs: vec![Crumb {
            name: ctx.repo.name.clone(),
            url: ctx.repo_url(),
        }],
        here: Some(short(&id)),
        subject: detail.info.subject.clone(),
        body: detail.body.clone(),
        meta,
        added: built.added,
        removed: built.removed,
        files,
        diff: built,
        patch_url: format!("{}.patch", ctx.commit_url(&detail.info.id)),
        tree_url,
        ws_toggle: Vec::new(),
        context_less: None,
        context_more: None,
        context: DEFAULT_CONTEXT,
    }
}

/// Five blocks split between additions and removals; fewer than five
/// changed lines leave the rest empty.
fn bar(stat: Option<(u32, u32)>) -> [&'static str; 5] {
    let mut out = ["none"; 5];
    let Some((added, removed)) = stat else {
        return out;
    };
    let total = added + removed;
    if total == 0 {
        return out;
    }
    let filled = total.min(5);
    let mut adds = ((filled * added) as f64 / total as f64).round() as u32;
    // Keep at least one block for each side that changed.
    if added > 0 && adds == 0 {
        adds = 1;
    }
    if removed > 0 && adds == filled {
        adds = filled - 1;
    }
    for (i, slot) in out.iter_mut().enumerate().take(filled as usize) {
        *slot = if (i as u32) < adds { "add" } else { "del" };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::bar;

    #[test]
    fn diffstat_bars() {
        assert_eq!(bar(None), ["none"; 5]);
        assert_eq!(bar(Some((0, 0))), ["none"; 5]);
        assert_eq!(bar(Some((2, 0))), ["add", "add", "none", "none", "none"]);
        assert_eq!(bar(Some((100, 1))), ["add", "add", "add", "add", "del"]);
        assert_eq!(bar(Some((1, 100))), ["add", "del", "del", "del", "del"]);
        assert_eq!(bar(Some((30, 20))), ["add", "add", "add", "del", "del"]);
    }
}
