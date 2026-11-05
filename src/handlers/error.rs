//! Error pages.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;

use super::{Layout, render};
use crate::state::AppState;

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub layout: Layout,
    pub code: u16,
    pub message: String,
}

pub fn page(state: &AppState, status: StatusCode, message: impl Into<String>) -> Response {
    let config = state.config.load();
    let code = status.as_u16();
    let page = ErrorPage {
        layout: Layout::new(state, &config, Some(&code.to_string())),
        code,
        message: message.into(),
    };
    let mut res = render(status, &page);
    if status.is_server_error() {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// 503 for load shedding; clients may retry shortly.
pub fn busy(state: &AppState) -> Response {
    let mut res = page(state, StatusCode::SERVICE_UNAVAILABLE, "SERVER BUSY");
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
    res
}

pub fn method_not_allowed(state: &AppState) -> Response {
    let mut res = page(state, StatusCode::METHOD_NOT_ALLOWED, "METHOD NOT ALLOWED");
    res.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
    res
}

pub async fn not_found(State(state): State<Arc<AppState>>) -> Response {
    page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND")
}
