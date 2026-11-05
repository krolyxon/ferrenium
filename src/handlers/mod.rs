//! Request handlers. Each page is an askama template that embeds a [`Layout`].

pub mod blob;
pub mod error;
pub mod index;
pub mod raw;
pub mod refs;
pub mod repo;
pub mod summary;
pub mod tree;

use std::sync::Arc;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::config::Config;
use crate::server::assets;
use crate::state::AppState;

/// Data shared by every page: site chrome, asset URLs and customization.
pub struct Layout {
    /// `server.base_path`, always ending in `/`.
    pub base: String,
    /// `{base}-/static/{hash}/`
    pub static_base: String,
    pub site_title: String,
    /// Full `<title>` text.
    pub page_title: String,
    /// Server default theme: `auto`, `dark` or `light`.
    pub theme: &'static str,
    /// Versioned URL of `/-/site.css`, when there is anything in it.
    pub site_css: Option<String>,
    pub header: Option<Arc<str>>,
    pub footer: Option<Arc<str>>,
    pub version: &'static str,
}

impl Layout {
    /// `title` is the page-specific part; the site title is appended.
    pub fn new(state: &AppState, config: &Config, title: Option<&str>) -> Self {
        let base = config.server.base_path.clone();
        let site_title = config.site.title.clone();
        Layout {
            static_base: format!("{base}-/static/{}/", state.assets.hash()),
            site_css: assets::site_css_version(config).map(|v| format!("{base}-/site.css?v={v}")),
            page_title: match title {
                Some(t) => format!("{t} · {site_title}"),
                None => site_title.clone(),
            },
            site_title,
            base,
            theme: config.ui.theme.as_str(),
            header: config.custom.header.clone(),
            footer: config.custom.footer.clone(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Render a template into an HTML response with the given status.
pub fn render(status: StatusCode, page: &impl Template) -> Response {
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(e) => {
            tracing::error!("template error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
        }
    }
}
