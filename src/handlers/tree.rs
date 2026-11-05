//! `/{repo}/tree/{rev}/{path…}` — directory listing, plus the directory's
//! README. Also provides the listing and README rendering used by the
//! summary page.

use std::sync::Arc;

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;
use gix::bstr::ByteSlice;

use super::repo::{Active, Crumb, Ctx, RepoNav, crumbs, redirect, set_cache_headers, short};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::ObjectId;
use crate::git::refs::Rev;
use crate::git::tree::{self, EntryKind, PathTarget, TreeEntry};
use crate::render::markdown::{self, Links};
use crate::render::{encode_path, human_size};

/// READMEs larger than this are not rendered inline.
const MAX_README: u64 = 512 * 1024;

pub struct TreeRow {
    pub name: String,
    /// `None` for submodules (nothing to browse here).
    pub url: Option<String>,
    pub icon: &'static str,
    /// Size for files, short commit id for submodules.
    pub meta: String,
}

#[derive(Template)]
#[template(path = "tree.html")]
pub struct TreePage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    /// Includes a leading `..` row below the root.
    pub rows: Vec<TreeRow>,
    pub entries: usize,
    pub tree_id: String,
    pub log_url: String,
    pub readme: Option<Readme>,
}

pub struct Readme {
    pub name: String,
    pub url: String,
    pub html: Arc<str>,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let tree_id = match tree::lookup(&ctx.git, rev.commit, &path) {
        Ok(Some(PathTarget::Tree(id))) => id,
        Ok(Some(PathTarget::Blob(..))) => return redirect(&ctx.url("blob", &rev.name, &path)),
        Ok(Some(PathTarget::Submodule(_))) => return ctx.not_found("PATH IS A SUBMODULE"),
        Ok(None) => {
            return ctx.not_found(&format!("PATH NOT FOUND IN {}", rev.name));
        }
        Err(e) => return ctx.internal_error(e),
    };

    let etag = ctx.etag(("tree", tree_id, &rev.name));
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let entries = match listing(ctx, tree_id) {
        Ok(entries) => entries,
        Err(e) => return ctx.internal_error(e),
    };

    let (crumbs, here) = crumbs(ctx, &rev.name, &path);
    let mut rows = rows(ctx, &rev, &path, &entries);
    if !path.is_empty() {
        let up = path.rsplit_once_str("/").map_or(&b""[..], |(dir, _)| dir);
        rows.insert(
            0,
            TreeRow {
                name: "..".into(),
                url: Some(ctx.url("tree", &rev.name, up)),
                icon: "folder",
                meta: String::new(),
            },
        );
    }
    let title = match &here {
        Some(_) => format!("{} · {}", path.to_str_lossy(), ctx.repo.name),
        None => ctx.repo.name.clone(),
    };
    let page = TreePage {
        layout: Layout::new(ctx.state, ctx.config, Some(&title)),
        nav: ctx.nav(Active::Tree, Some(&rev), |name| {
            ctx.url("tree", name, &path)
        }),
        crumbs,
        here,
        rows,
        entries: entries.len(),
        tree_id: short(&tree_id.to_string()),
        log_url: ctx.url("log", &rev.name, &path),
        readme: dir_readme(ctx, &rev, &path, &entries),
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}

/// Cached listing of a tree.
pub fn listing(ctx: &Ctx, tree_id: ObjectId) -> anyhow::Result<Arc<Vec<TreeEntry>>> {
    ctx.state
        .cache
        .tree(tree_id, || tree::list(&ctx.git, tree_id))
}

pub fn rows(ctx: &Ctx, rev: &Rev, dir: &[u8], entries: &[TreeEntry]) -> Vec<TreeRow> {
    entries
        .iter()
        .map(|entry| {
            let mut path = dir.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            let (action, icon) = match entry.kind {
                EntryKind::Tree => ("tree", "folder"),
                EntryKind::Symlink => ("blob", "link"),
                EntryKind::Submodule => ("", "package"),
                EntryKind::Blob | EntryKind::Executable => ("blob", "file"),
            };
            TreeRow {
                name: entry.name.to_str_lossy().into_owned(),
                url: (!action.is_empty()).then(|| ctx.url(action, &rev.name, &path)),
                icon,
                meta: match entry.kind {
                    EntryKind::Submodule => format!("@ {}", short(&entry.oid.to_string())),
                    _ => entry.size.map(human_size).unwrap_or_default(),
                },
            }
        })
        .collect()
}

/// Names tried for a directory README, in order of preference.
const README_NAMES: &[&str] = &["readme.md", "readme.markdown", "readme", "readme.txt"];

fn dir_readme(ctx: &Ctx, rev: &Rev, dir: &[u8], entries: &[TreeEntry]) -> Option<Readme> {
    let entry = README_NAMES.iter().find_map(|want| {
        entries.iter().find(|e| {
            e.kind.is_blob()
                && e.kind != EntryKind::Symlink
                && e.name.eq_ignore_ascii_case(want.as_bytes())
        })
    })?;
    let name = entry.name.to_str_lossy().into_owned();
    readme(ctx, rev, dir, &name, entry.oid)
}

/// Render a README blob: Markdown as HTML, anything else as plain text.
pub fn readme(ctx: &Ctx, rev: &Rev, dir: &[u8], name: &str, oid: ObjectId) -> Option<Readme> {
    if tree::size(&ctx.git, oid).ok()? > MAX_README {
        return None;
    }
    let links = Links {
        repo_url: ctx.repo_url(),
        rev: encode_path(&rev.name),
        dir: dir.to_vec(),
    };
    let is_md = markdown::is_markdown(name);
    let key = Key::Markdown {
        blob: oid,
        context: if is_md { links.key() } else { String::new() },
    };
    let html = ctx
        .state
        .cache
        .html(key, || -> anyhow::Result<String> {
            let data = tree::blob(&ctx.git, oid)?;
            let text = String::from_utf8_lossy(&data);
            Ok(if is_md {
                markdown::render(&text, &links)
            } else {
                markdown::render_plain(&text)
            })
        })
        .ok()?;
    let mut path = dir.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name.as_bytes());
    Some(Readme {
        name: name.to_string(),
        url: ctx.url("blob", &rev.name, &path),
        html,
    })
}
