//! `/{repo}/raw/{rev}/{path…}` — file contents as stored.
//!
//! Nothing served here may run in our origin: text (HTML and SVG included)
//! is `text/plain`, only raster images keep their type, and every response
//! is sandboxed and `nosniff`. `?download` sends an attachment instead.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use gix::bstr::ByteSlice;

use super::repo::{Ctx, set_cache_headers};
use crate::git::tree::{self, PathTarget};
use crate::render::encode_path;

pub const RAW_CSP: &str = "default-src 'none'; img-src 'self'; sandbox";

/// Image types safe to serve with their real content type.
const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
];

pub fn serve(ctx: &Ctx, segments: &[Vec<u8>]) -> Response {
    let (rev, path) = match ctx.resolve(segments) {
        Ok(found) => found,
        Err(res) => return *res,
    };
    let oid = match tree::lookup(&ctx.git, rev.commit, &path) {
        Ok(Some(PathTarget::Blob(oid, _))) => oid,
        Ok(Some(_)) => return ctx.not_found("NOT A FILE"),
        Ok(None) => return ctx.not_found(&format!("PATH NOT FOUND IN {}", rev.name)),
        Err(e) => return ctx.internal_error(e),
    };
    let download = ctx.query.download.is_some();
    let etag = format!("W/\"{oid}{}\"", if download { "-d" } else { "" });
    let immutable = rev.is_immutable();
    if let Some(res) = ctx.not_modified(&etag, immutable) {
        return res;
    }
    let data = match tree::blob(&ctx.git, oid) {
        Ok(data) => data,
        Err(e) => return ctx.internal_error(e),
    };

    let name = path
        .rsplit_once_str("/")
        .map_or(&path[..], |(_, n)| n)
        .to_str_lossy()
        .into_owned();
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let is_text = !data[..data.len().min(8000)].contains(&0);
    let content_type = match IMAGE_TYPES.iter().find(|(e, _)| *e == ext) {
        Some((_, mime)) => mime,
        None if download => "application/octet-stream",
        None if is_text => "text/plain; charset=utf-8",
        None => "application/octet-stream",
    };

    let mut res = Body::from(data).into_response();
    *res.status_mut() = StatusCode::OK;
    let headers = res.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if download {
        let ascii: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() && c != '"' && c != '\\' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let value = format!(
            "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
            encode_path(&name)
        );
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
    set_cache_headers(&mut res, &etag, immutable);
    res
}
