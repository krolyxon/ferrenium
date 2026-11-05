//! Route-level tests driven through `tower::ServiceExt::oneshot`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use ferrenium::config::Config;
use ferrenium::server;
use ferrenium::state::{AppState, Overrides};
use tower::ServiceExt;

fn app_with(toml: &str) -> (axum::Router, std::sync::Arc<AppState>) {
    let mut config = Config::parse(toml).expect("parse");
    config.finish().expect("valid");
    let state = AppState::new(config, Overrides::default());
    (server::app(state.clone()), state)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn index_renders_styled_empty_page() {
    let (app, state) =
        app_with("[site]\ntitle = \"krolyxon\"\ndescription = \"Personal <repos>\"\n");
    let (status, headers, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

    let static_base = format!("/-/static/{}/", state.assets.hash());
    assert!(
        body.contains(&format!("{static_base}css/app.css")),
        "{body}"
    );
    assert!(body.contains(&format!("{static_base}js/theme.js")));
    assert!(body.contains(r#"data-default-theme="auto""#));
    assert!(!body.contains("data-theme=\"")); // auto: OS decides
    assert!(body.contains("krolyxon"));
    // Escaped by askama.
    assert!(body.contains("Personal &#60;repos&#62;") || body.contains("Personal &lt;repos&gt;"));
    assert!(body.contains("No repositories"));
    // No inline scripts or styles (CSP).
    assert!(!body.contains("<script>"));
    assert!(!body.contains("<style"));
    assert!(!body.contains("style=\""));
}

#[tokio::test]
async fn forced_theme_is_rendered_server_side() {
    let (app, _) = app_with("[ui]\ntheme = \"light\"\n");
    let (_, _, body) = get(&app, "/").await;
    assert!(body.contains(r#"data-default-theme="light" data-theme="light""#));
}

#[tokio::test]
async fn health() {
    let (app, _) = app_with("");
    let (status, headers, body) = get(&app, "/-/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok\n");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn static_assets() {
    let (app, state) = app_with("");
    let hash = state.assets.hash().to_string();

    let (status, headers, body) = get(&app, &format!("/-/static/{hash}/css/app.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    let expected = if cfg!(debug_assertions) {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    assert_eq!(headers[header::CACHE_CONTROL], expected);
    assert!(body.contains("--text-display"));
    assert!(body.contains("url(../fonts/space-mono-latin-400.woff2)"));

    let (status, headers, _) = get(
        &app,
        &format!("/-/static/{hash}/fonts/space-mono-latin-400.woff2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "font/woff2");

    // Stale hash: still served, never cached as immutable.
    let (status, headers, _) = get(&app, "/-/static/000000000000/js/theme.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");

    let (status, _, _) = get(&app, &format!("/-/static/{hash}/nope.css")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&app, &format!("/-/static/{hash}/../Cargo.toml")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn every_referenced_asset_exists() {
    let (app, _) = app_with("");
    let (_, _, page) = get(&app, "/").await;
    let mut urls: Vec<String> = page
        .split(['"', '\''])
        .filter(|s| s.starts_with("/-/static/"))
        .map(str::to_string)
        .collect();
    // Fonts referenced from the stylesheet resolve relative to css/, with
    // `..` collapsed the way a browser does before requesting.
    let css_url = urls
        .iter()
        .find(|u| u.ends_with("css/app.css"))
        .unwrap()
        .clone();
    let (_, _, css) = get(&app, &css_url).await;
    let static_root = css_url.trim_end_matches("css/app.css");
    for part in css.split("url(").skip(1) {
        let rel = part.split(')').next().unwrap();
        let rel = rel.strip_prefix("../").expect("font urls are ../fonts/…");
        urls.push(format!("{static_root}{rel}"));
    }
    assert!(urls.len() > 10, "{urls:?}");
    for url in urls {
        let (status, _, _) = get(&app, &url).await;
        assert_eq!(status, StatusCode::OK, "{url}");
    }
}

#[tokio::test]
async fn not_found_page() {
    let (app, _) = app_with("");
    let (status, headers, body) = get(&app, "/no/such/thing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(body.contains(">404<"));
    assert!(body.contains("[ERROR: PAGE NOT FOUND]"));
}

#[tokio::test]
async fn base_path_prefixes_routes_and_links() {
    let (app, state) = app_with("[server]\nbase_path = \"/git\"\n");
    let (status, _, body) = get(&app, "/git/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!(
        "/git/-/static/{}/css/app.css",
        state.assets.hash()
    )));
    assert!(body.contains(r#"href="/git/""#));

    let (status, headers, _) = get(&app, "/git").await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers[header::LOCATION], "/git/");

    assert_eq!(get(&app, "/git/-/health").await.0, StatusCode::OK);
    assert_eq!(get(&app, "/-/health").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&app, "/gitx/").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn site_css_carries_code_font_and_custom_css() {
    let (app, _) = app_with("");
    let (_, _, body) = get(&app, "/").await;
    assert!(!body.contains("/-/site.css"));

    let (app, _) = app_with("[ui]\ncode_font = \"Berkeley Mono\"\n");
    let (_, _, body) = get(&app, "/").await;
    let href = body
        .split('"')
        .find(|s| s.starts_with("/-/site.css?v="))
        .expect("site.css link");
    let (status, _, css) = get(&app, href).await;
    assert_eq!(status, StatusCode::OK);
    assert!(css.contains("--font-code: \"Berkeley Mono\""));
}

#[tokio::test]
async fn compression_skips_fonts() {
    let (app, state) = app_with("");
    let hash = state.assets.hash().to_string();
    let req = |uri: String| {
        Request::get(uri)
            .header(header::ACCEPT_ENCODING, "br, gzip")
            .body(Body::empty())
            .unwrap()
    };
    let res = app
        .clone()
        .oneshot(req(format!("/-/static/{hash}/css/app.css")))
        .await
        .unwrap();
    assert_eq!(res.headers()[header::CONTENT_ENCODING], "gzip");
    let res = app
        .clone()
        .oneshot(req(format!(
            "/-/static/{hash}/fonts/space-mono-latin-400.woff2"
        )))
        .await
        .unwrap();
    assert!(res.headers().get(header::CONTENT_ENCODING).is_none());
}
