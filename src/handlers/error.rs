//! Error pages.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
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
    render(status, &page)
}

pub async fn not_found(State(state): State<Arc<AppState>>) -> Response {
    page(&state, StatusCode::NOT_FOUND, "PAGE NOT FOUND")
}
