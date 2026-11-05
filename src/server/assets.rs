//! Embedded static assets served under `/-/static/<hash>/…`.
//!
//! One hash covers the whole asset set, so files can reference each other
//! with relative URLs (`url(../fonts/x.woff2)`) and still be cached forever.
//! In debug builds rust-embed reads from disk; responses are then `no-cache`
//! so CSS/JS edits show up on reload.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Embedded;

/// Parts concatenated (in order) into the virtual `css/app.css`.
const CSS_BUNDLE: &[&str] = &[
    "css/fonts.css",
    "css/tokens.css",
    "css/base.css",
    "css/components.css",
    "css/repo.css",
    "css/syntax.css",
];
const CSS_BUNDLE_PATH: &str = "css/app.css";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";

pub struct Assets {
    hash: String,
    /// Prebuilt bundle (release builds only; debug rebuilds per request).
    bundle: Option<Bytes>,
}

impl Assets {
    pub fn new() -> Self {
        let mut paths: Vec<_> = Embedded::iter().collect();
        paths.sort();
        // SipHash with fixed keys: deterministic for a given binary, which is
        // all a cache-busting token needs.
        let mut hasher = std::hash::DefaultHasher::new();
        for path in &paths {
            path.hash(&mut hasher);
            if let Some(file) = Embedded::get(path) {
                file.metadata.sha256_hash().hash(&mut hasher);
            }
        }
        let hash = format!("{:016x}", hasher.finish())[..12].to_string();
        let bundle = (!cfg!(debug_assertions)).then(build_bundle);
        Assets { hash, bundle }
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Bytes for an asset path, including the virtual CSS bundle.
    pub fn get(&self, path: &str) -> Option<Bytes> {
        if path == CSS_BUNDLE_PATH {
            return Some(self.bundle.clone().unwrap_or_else(build_bundle));
        }
        let file = Embedded::get(path)?;
        Some(match file.data {
            std::borrow::Cow::Borrowed(b) => Bytes::from_static(b),
            std::borrow::Cow::Owned(v) => Bytes::from(v),
        })
    }
}

impl Default for Assets {
    fn default() -> Self {
        Self::new()
    }
}

fn build_bundle() -> Bytes {
    let mut out = Vec::new();
    for part in CSS_BUNDLE {
        let file = Embedded::get(part).unwrap_or_else(|| panic!("missing embedded {part}"));
        out.extend_from_slice(&file.data);
        out.push(b'\n');
    }
    Bytes::from(out)
}

pub fn content_type(path: &str) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let value = match (mime.type_(), mime.subtype()) {
        (mime_guess::mime::TEXT, _) | (_, mime_guess::mime::JAVASCRIPT) => {
            format!("{mime}; charset=utf-8")
        }
        _ => mime.to_string(),
    };
    HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

/// `GET /-/static/{hash}/{*path}`
pub async fn serve(
    State(state): State<Arc<AppState>>,
    Path((hash, path)): Path<(String, String)>,
) -> Response {
    let Some(body) = state.assets.get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // A stale hash (e.g. a page rendered before an upgrade) still gets the
    // current file, but must not be cached under the old URL.
    let cache = if !cfg!(debug_assertions) && hash == state.assets.hash() {
        IMMUTABLE
    } else {
        NO_CACHE
    };
    (
        [
            (header::CONTENT_TYPE, content_type(&path)),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response()
}

/// `GET /-/site.css?v=…` — per-site overrides: the configured code font and
/// the contents of `site.custom_css`. Only linked when there is something to
/// serve; versioned by [`site_css_version`].
pub async fn site_css(State(state): State<Arc<AppState>>) -> Response {
    let config = state.config.load();
    let Some(css) = site_css_body(&config) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(NO_CACHE)),
        ],
        css,
    )
        .into_response()
}

pub fn site_css_body(config: &crate::config::Config) -> Option<String> {
    let mut css = String::new();
    if config.ui.code_font != crate::config::DEFAULT_CODE_FONT {
        // The font name was validated to contain no quotes or CSS syntax.
        css.push_str(&format!(
            ":root {{ --font-code: \"{}\", var(--font-mono); }}\n",
            config.ui.code_font
        ));
    }
    if let Some(custom) = &config.custom.css {
        css.push_str(custom);
    }
    (!css.is_empty()).then_some(css)
}

/// Short content hash for the `?v=` query of the site stylesheet link.
pub fn site_css_version(config: &crate::config::Config) -> Option<String> {
    let body = site_css_body(config)?;
    let mut hasher = std::hash::DefaultHasher::new();
    body.hash(&mut hasher);
    Some(format!("{:08x}", hasher.finish() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_contains_all_parts_and_hash_is_stable() {
        let a = Assets::new();
        let b = Assets::new();
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a.hash().len(), 12);
        let bundle = a.get(CSS_BUNDLE_PATH).unwrap();
        let text = std::str::from_utf8(&bundle).unwrap();
        assert!(text.contains("@font-face"));
        assert!(text.contains("--text-display"));
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("css/app.css"), "text/css; charset=utf-8");
        assert_eq!(
            content_type("js/theme.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("fonts/x.woff2"), "font/woff2");
        assert_eq!(content_type("icons/favicon.svg"), "image/svg+xml");
    }
}
