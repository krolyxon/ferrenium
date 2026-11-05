//! `/{repo}/blob/{rev}/{path…}` — a file: highlighted code with line
//! anchors, rendered Markdown, inline images.

use std::sync::Arc;

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;
use gix::bstr::ByteSlice;

use super::repo::{Active, Crumb, Ctx, RepoNav, crumbs, redirect, set_cache_headers};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::tree::{self, EntryKind, PathTarget};
use crate::render::markdown::{self, Links};
use crate::render::{encode_path, highlight, human_size};

/// Extensions shown inline as images (served by the raw endpoint).
const IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico"];

pub enum Body {
    Code { lines: Arc<Vec<String>> },
    Markdown { html: Arc<str> },
    Image { src: String },
    Symlink { target: String, url: Option<String> },
    Binary,
    TooLarge,
    Empty,
}

pub struct ViewToggle {
    pub preview_url: String,
    pub source_url: String,
    pub source: bool,
}

#[derive(Template)]
#[template(path = "blob.html")]
pub struct BlobPage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub crumbs: Vec<Crumb>,
    pub here: Option<String>,
    /// `412 lines`, `12.4 KB`, `Rust`.
    pub facts: Vec<String>,
    pub raw_url: String,
    pub download_url: String,
    pub toggle: Option<ViewToggle>,
    pub body: Body,
}

pub fn page(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let (oid, kind) = match tree::lookup(&ctx.git, rev.commit, &path) {
        Ok(Some(PathTarget::Blob(oid, kind))) => (oid, kind),
        Ok(Some(PathTarget::Tree(_))) => return redirect(&ctx.url("tree", &rev.name, &path)),
        Ok(Some(PathTarget::Submodule(_))) => return ctx.not_found("PATH IS A SUBMODULE"),
        Ok(None) => return ctx.not_found(&format!("PATH NOT FOUND IN {}", rev.name)),
        Err(e) => return ctx.internal_error(e),
    };
    let source = ctx.query.view.as_deref() == Some("source");
    let etag = ctx.etag(("blob", oid, &rev.name, source));
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let size = match tree::size(&ctx.git, oid) {
        Ok(size) => size,
        Err(e) => return ctx.internal_error(e),
    };

    let path_str = path.to_str_lossy().into_owned();
    let name = path_str.rsplit('/').next().unwrap_or(&path_str).to_string();
    let dir = path.rsplit_once_str("/").map_or(&b""[..], |(d, _)| d);
    let raw_url = ctx.url("raw", &rev.name, &path);
    let mut lines_fact = None;
    let mut language = None;
    let mut toggle = None;
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();

    let body = if size == 0 {
        Body::Empty
    } else if IMAGES.contains(&ext.as_str()) && kind != EntryKind::Symlink {
        Body::Image {
            src: raw_url.clone(),
        }
    } else if size > ctx.config.ui.max_blob_kb * 1024 {
        Body::TooLarge
    } else {
        let data = match tree::blob(&ctx.git, oid) {
            Ok(data) => data,
            Err(e) => return ctx.internal_error(e),
        };
        if kind == EntryKind::Symlink {
            let target = data.to_str_lossy().into_owned();
            let url = resolve_link(dir, &data).map(|p| ctx.url("blob", &rev.name, &p));
            Body::Symlink { target, url }
        } else if data[..data.len().min(8000)].contains(&0) {
            Body::Binary
        } else {
            let text = String::from_utf8_lossy(&data);
            lines_fact = Some(line_count(&text));
            let is_md = markdown::is_markdown(&name);
            if is_md {
                let base = ctx.url("blob", &rev.name, &path);
                toggle = Some(ViewToggle {
                    source_url: format!("{base}?view=source"),
                    preview_url: base,
                    source,
                });
            }
            if is_md && !source {
                let links = Links {
                    repo_url: ctx.repo_url(),
                    rev: encode_path(&rev.name),
                    dir: dir.to_vec(),
                };
                let key = Key::Markdown {
                    blob: oid,
                    context: links.key(),
                };
                let html = ctx.state.cache.html(key, || {
                    Ok::<_, std::convert::Infallible>(markdown::render(&text, &links))
                });
                Body::Markdown {
                    html: html.unwrap_or_else(|never| match never {}),
                }
            } else {
                let first_line = text.lines().next().unwrap_or_default();
                let syntax = highlight::detect(&path_str, first_line);
                language = syntax.map(|s| s.name.clone());
                let lines = match syntax {
                    Some(syntax) if size <= ctx.config.ui.max_highlight_kb * 1024 => {
                        let key = Key::Highlight {
                            blob: oid,
                            syntax: syntax.name.clone(),
                        };
                        ctx.state
                            .cache
                            .lines(key, || {
                                Ok::<_, std::convert::Infallible>(highlight::lines(&text, syntax))
                            })
                            .unwrap_or_else(|never| match never {})
                    }
                    _ => Arc::new(highlight::plain_lines(&text)),
                };
                Body::Code { lines }
            }
        }
    };
    let facts = [lines_fact, Some(human_size(size)), language]
        .into_iter()
        .flatten()
        .collect();

    let (crumbs, _) = crumbs(ctx, &rev.name, &path);
    let page = BlobPage {
        layout: Layout::new(
            ctx.state,
            ctx.config,
            Some(&format!("{path_str} · {}", ctx.repo.name)),
        ),
        nav: ctx.nav(Active::Tree, Some(&rev), |name| {
            ctx.url("blob", name, &path)
        }),
        crumbs,
        here: Some(name),
        facts,
        download_url: format!("{raw_url}?download"),
        raw_url,
        toggle,
        body,
    };
    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, immutable);
    res
}

fn line_count(text: &str) -> String {
    let n = text.lines().count();
    format!("{n} {}", if n == 1 { "line" } else { "lines" })
}

/// Resolve a relative symlink target against the link's directory. `None`
/// for absolute targets or ones leaving the repository.
fn resolve_link(dir: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if target.starts_with(b"/") {
        return None;
    }
    let mut parts: Vec<&[u8]> = dir
        .split(|b| *b == b'/')
        .filter(|p| !p.is_empty())
        .collect();
    for part in target.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_targets() {
        assert_eq!(resolve_link(b"a/b", b"../c.txt").unwrap(), b"a/c.txt");
        assert_eq!(resolve_link(b"", b"./x").unwrap(), b"x");
        assert!(resolve_link(b"", b"../x").is_none());
        assert!(resolve_link(b"a", b"/etc/passwd").is_none());
    }
}
