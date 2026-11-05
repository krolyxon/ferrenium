//! Security headers applied to every response (unless a handler set its own).

use axum::http::{HeaderName, HeaderValue, header};
use tower_http::set_header::SetResponseHeaderLayer;

pub const PAGE_CSP: &str = "default-src 'self'; img-src 'self' https: data:; script-src 'self'; \
     object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

pub fn layers() -> [SetResponseHeaderLayer<HeaderValue>; 4] {
    let set = |name: HeaderName, value: &'static str| {
        SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
    };
    [
        set(header::CONTENT_SECURITY_POLICY, PAGE_CSP),
        set(header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        set(header::REFERRER_POLICY, "same-origin"),
        set(header::X_FRAME_OPTIONS, "DENY"),
    ]
}
