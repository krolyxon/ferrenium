//! Browsing pages (summary, tree, blob, raw, refs) against real repos.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{Rich, bare, rich_repo};
use ferrenium::config::Config;
use ferrenium::server;
use ferrenium::state::{AppState, Overrides};
use tower::ServiceExt;

struct Site {
    _root: tempfile::TempDir,
    app: axum::Router,
    rich: Rich,
}

fn site(extra: &str) -> Site {
    let root = tempfile::tempdir().unwrap();
    let rich = rich_repo(root.path(), "rich.git");
    rich_repo(root.path(), "forks/nested.git");
    bare(root.path(), "empty.git", &[]);
    let toml = format!(
        "[repos]\nscan_path = {:?}\n[watch]\nenabled = false\n\
         [site]\nclone_urls = [\"https://git.example.com/{{name}}\", \"git@example.com:{{name}}\"]\n\
         [ui]\nmax_blob_kb = 2\n{extra}",
        root.path().to_str().unwrap()
    );
    let mut config = Config::parse(&toml).expect("parse");
    config.finish().expect("valid");
    let state = AppState::new(config, Overrides::default());
    let config = state.config.load_full();
    state.index.full_scan(&config);
    Site {
        app: server::app(state),
        rich,
        _root: root,
    }
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Res {
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(&name)
            .map(|v| v.to_str().unwrap())
            .unwrap_or_default()
    }
}

async fn fetch(app: &axum::Router, uri: &str, etag: Option<&str>) -> Res {
    let mut req = Request::get(uri);
    if let Some(etag) = etag {
        req = req.header(header::IF_NONE_MATCH, etag);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

async fn ok(app: &axum::Router, uri: &str) -> Res {
    let res = fetch(app, uri, None).await;
    assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.body);
    assert_csp_clean(&res.body);
    res
}

/// Pages must work under `script-src 'self'` and no inline styles.
fn assert_csp_clean(html: &str) {
    assert!(!html.contains("<script>"), "inline script");
    assert!(!html.contains("<style"), "inline style");
    assert!(!html.contains("style=\""), "style attribute");
}

#[tokio::test]
async fn summary_shows_tree_readme_and_rail() {
    let s = site("");
    let res = ok(&s.app, "/rich").await;
    let html = &res.body;
    assert!(
        html.contains(r#"<h1 class="display-md repo-title">rich</h1>"#),
        "{html}"
    );
    // Root tree: directories first.
    let docs = html.find(">docs<").expect("docs row");
    let readme = html.find(">README.md<").expect("README row");
    assert!(docs < readme);
    assert!(html.contains(r#"href="/rich/tree/main/src""#));
    assert!(html.contains(r#"href="/rich/blob/main/README.md""#));
    // README: rendered, sanitized, relative links rewritten.
    assert!(
        html.contains(r#"href="/rich/blob/main/docs/guide.md""#),
        "{html}"
    );
    assert!(html.contains(r#"src="/rich/raw/main/img/logo.png""#));
    assert!(!html.contains("alert(1)"));
    // Rail.
    assert!(html.contains("<dd>2</dd>"), "commit count: {html}");
    assert!(html.contains("https://git.example.com/rich"));
    assert!(html.contains("git@example.com:rich"));
    assert!(html.contains(">HTTPS<") && html.contains(">SSH<"));
    assert!(html.contains("add everything"));
    assert!(html.contains(&format!(r#"href="/rich/tree/{}""#, s.rich.head)));
    // Nav + picker.
    assert!(html.contains(r#"class="label active" href="/rich" aria-current="page">Summary"#));
    assert!(html.contains(r#"href="/rich/tree/feature/x""#));
    assert!(html.contains(r#"href="/rich/tree/v1.1""#));

    // Conditional request.
    let etag = res.header(header::ETAG).to_string();
    assert!(etag.starts_with("W/\""), "{etag}");
    assert_eq!(res.header(header::CACHE_CONTROL), "no-cache");
    let again = fetch(&s.app, "/rich", Some(&etag)).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
    assert_eq!(again.header(header::ETAG), etag);
}

#[tokio::test]
async fn empty_repo_summary() {
    let s = site("");
    let res = ok(&s.app, "/empty").await;
    assert!(res.body.contains("Empty repository"));
    assert!(res.body.contains("git@example.com:empty"));
    let tree = fetch(&s.app, "/empty/tree", None).await;
    assert_eq!(tree.status, StatusCode::NOT_FOUND);
    assert!(tree.body.contains("[ERROR: REPOSITORY IS EMPTY]"));
}

#[tokio::test]
async fn dispatcher_matches_repos_and_revisions() {
    let s = site("");
    // Slashy repo name.
    ok(&s.app, "/forks/nested").await;
    ok(&s.app, "/forks/nested/tree/main/src").await;
    // `.git` suffix redirects.
    let res = fetch(&s.app, "/forks/nested.git", None).await;
    assert_eq!(res.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(res.header(header::LOCATION), "/forks/nested");
    // Slashy branch, tags, full and short ids.
    let res = ok(&s.app, "/rich/tree/feature/x").await;
    assert!(res.body.contains("only-on-feature.txt"));
    let res = ok(&s.app, "/rich/blob/feature/x/only-on-feature.txt").await;
    assert!(res.body.contains("feature"));
    let res = ok(&s.app, "/rich/tree/v1.1").await;
    assert!(!res.body.contains("data.bin"), "v1.1 is the first commit");
    ok(&s.app, "/rich/blob/v1.0/data.bin").await;
    let full = ok(&s.app, &format!("/rich/tree/{}", s.rich.first)).await;
    assert_eq!(full.header(header::CACHE_CONTROL), "public, max-age=86400");
    let short = ok(&s.app, &format!("/rich/tree/{}", &s.rich.feature[..8])).await;
    assert!(short.body.contains("only-on-feature.txt"));
    assert_eq!(short.header(header::CACHE_CONTROL), "no-cache");

    for (uri, message) in [
        ("/nope", "PAGE NOT FOUND"),
        ("/rich/tree/nope", "UNKNOWN REVISION"),
        ("/rich/tree/main/nope", "PATH NOT FOUND IN main"),
        ("/rich/blob/main/vendor/lib", "PATH IS A SUBMODULE"),
        ("/rich/log", "PAGE NOT FOUND"),
        ("/rich/tree/main/../x", "PAGE NOT FOUND"),
        ("/-/nope", "PAGE NOT FOUND"),
    ] {
        let res = fetch(&s.app, uri, None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(
            res.body.contains(&format!("[ERROR: {message}]")),
            "{uri}: {}",
            res.body
        );
    }
}

#[tokio::test]
async fn tree_page() {
    let s = site("");
    // Submodule: no link, short commit id instead of a size.
    let res = ok(&s.app, "/rich/tree/main/vendor").await;
    let row = res.body.split("<li class=\"tree-row\">").nth(2).unwrap();
    assert!(row.contains(r#"<span class="tree-name">"#), "{row}");
    assert!(row.contains(&format!("@ {}", &s.rich.first[..7])), "{row}");

    let res = ok(&s.app, "/rich/tree/main/docs").await;
    let html = &res.body;
    assert!(
        html.contains(r#"<a href="/rich/tree/main">rich</a>"#),
        "{html}"
    );
    assert!(html.contains(r#"<span class="crumb-here">docs</span>"#));
    assert!(html.contains(r#"href="/rich/tree/main">"#)); // ".." row
    assert!(html.contains("2 entries · Tree "));
    // Plain-text directory README, escaped.
    assert!(
        html.contains("Plain docs readme &lt;b&gt;not html&lt;/b&gt;"),
        "{html}"
    );
    // Picker keeps the path.
    assert!(html.contains(r#"href="/rich/tree/v1.0/docs""#));
    // Unicode, spaces and `#` are percent-encoded.
    let res = ok(&s.app, "/rich/tree/main").await;
    assert!(
        res.body.contains("/rich/tree/main/%C3%BCn%C3%AFcode%20dir"),
        "{}",
        res.body
    );
    let res = ok(&s.app, "/rich/tree/main/%C3%BCn%C3%AFcode%20dir").await;
    assert!(
        res.body
            .contains("/rich/blob/main/%C3%BCn%C3%AFcode%20dir/f%C3%AFle%20%231.txt")
    );
    // ETag round trip.
    let etag = res.header(header::ETAG).to_string();
    let again = fetch(
        &s.app,
        "/rich/tree/main/%C3%BCn%C3%AFcode%20dir",
        Some(&etag),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);

    // A file in tree view redirects to the blob view, and vice versa.
    let res = fetch(&s.app, "/rich/tree/main/src/main.rs", None).await;
    assert_eq!(res.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(res.header(header::LOCATION), "/rich/blob/main/src/main.rs");
    let res = fetch(&s.app, "/rich/blob/main/src", None).await;
    assert_eq!(res.header(header::LOCATION), "/rich/tree/main/src");
}

#[tokio::test]
async fn blob_page_variants() {
    let s = site("");
    // Highlighted code with line anchors.
    let res = ok(&s.app, "/rich/blob/main/src/main.rs").await;
    let html = &res.body;
    assert!(html.contains("4 lines · 45 B · Rust"), "{html}");
    assert!(html.contains(r##"<tr id="L2"><td class="ln"><a href="#L2">2</a></td>"##));
    assert!(html.contains(r#"<span class="hl-k">fn</span>"#), "{html}");
    assert!(html.contains("&lt;hi&gt;"));
    assert!(html.contains(r#"href="/rich/raw/main/src/main.rs""#));
    assert!(html.contains(r#"href="/rich/raw/main/src/main.rs?download""#));
    assert!(html.contains(r#"<span class="crumb-here">main.rs</span>"#));

    // Markdown: preview by default, source on request.
    let res = ok(&s.app, "/rich/blob/main/docs/guide.md").await;
    assert!(
        res.body
            .contains(r#"<article class="markdown blob-markdown">"#)
    );
    assert!(
        res.body.contains(r#"href="/rich/blob/main/README.md""#),
        "{}",
        res.body
    );
    let res = ok(&s.app, "/rich/blob/main/docs/guide.md?view=source").await;
    assert!(res.body.contains(r#"id="L1""#));
    assert!(res.body.contains(r#"aria-current="true">Source"#));

    let res = ok(&s.app, "/rich/blob/main/img/logo.png").await;
    assert!(
        res.body
            .contains(r#"<img src="/rich/raw/main/img/logo.png""#)
    );
    let res = ok(&s.app, "/rich/blob/main/data.bin").await;
    assert!(res.body.contains("[Binary file]"));
    let res = ok(&s.app, "/rich/blob/main/big.txt").await;
    assert!(
        res.body.contains("[Too large to display]"),
        "max_blob_kb = 2"
    );
    let res = ok(&s.app, "/rich/blob/main/link-to-main").await;
    assert!(
        res.body
            .contains(r#"href="/rich/blob/main/src/main.rs">src/main.rs</a>"#)
    );
    let res = ok(
        &s.app,
        "/rich/blob/main/%C3%BCn%C3%AFcode%20dir/f%C3%AFle%20%231.txt",
    )
    .await;
    assert!(res.body.contains("fïle #1.txt"));
    // HTML source is shown as text, never rendered.
    let res = ok(&s.app, "/rich/blob/main/page.html").await;
    assert!(res.body.contains("alert") && !res.body.contains("<script>alert"));
}

#[tokio::test]
async fn raw_is_sandboxed() {
    let s = site("");
    let res = fetch(&s.app, "/rich/raw/main/page.html", None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.header(header::CONTENT_TYPE),
        "text/plain; charset=utf-8"
    );
    assert_eq!(res.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert!(
        res.header(header::CONTENT_SECURITY_POLICY)
            .contains("sandbox")
    );
    assert_eq!(res.body, "<html><script>alert(1)</script></html>\n");

    let res = fetch(&s.app, "/rich/raw/main/img/logo.png", None).await;
    assert_eq!(res.header(header::CONTENT_TYPE), "image/png");
    let res = fetch(&s.app, "/rich/raw/main/data.bin", None).await;
    assert_eq!(res.header(header::CONTENT_TYPE), "application/octet-stream");

    let res = fetch(&s.app, "/rich/raw/main/page.html?download", None).await;
    assert_eq!(
        res.header(header::CONTENT_DISPOSITION),
        "attachment; filename=\"page.html\"; filename*=UTF-8''page.html"
    );
    let res = fetch(
        &s.app,
        "/rich/raw/main/%C3%BCn%C3%AFcode%20dir/f%C3%AFle%20%231.txt?download",
        None,
    )
    .await;
    assert!(
        res.header(header::CONTENT_DISPOSITION)
            .contains("filename*=UTF-8''f%C3%AFle%20%231.txt")
    );

    // Raw by full id is cacheable; ETag revalidates.
    let uri = format!("/rich/raw/{}/src/main.rs", s.rich.head);
    let res = fetch(&s.app, &uri, None).await;
    assert_eq!(res.header(header::CACHE_CONTROL), "public, max-age=86400");
    let etag = res.header(header::ETAG).to_string();
    assert_eq!(
        fetch(&s.app, &uri, Some(&etag)).await.status,
        StatusCode::NOT_MODIFIED
    );

    assert_eq!(
        fetch(&s.app, "/rich/raw/main/src", None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn refs_page() {
    let s = site("");
    let res = ok(&s.app, "/rich/refs").await;
    let html = &res.body;
    let main = html.find(r#"href="/rich/tree/main""#).expect("main");
    let feature = html
        .find(r#"href="/rich/tree/feature/x""#)
        .expect("feature");
    assert!(main < feature, "default branch first");
    assert!(html.contains(r#"<span class="chip">Default</span>"#));
    assert!(html.contains("Release 1.1"), "annotated tag subject");
    assert!(html.contains(r#"href="/rich/tree/v1.0""#));
    assert!(html.contains(r#"<span class="crumb-here">Refs</span>"#));
}

#[tokio::test]
async fn base_path_prefixes_repo_links() {
    let s = site("[server]\nbase_path = \"/git/\"\n");
    let res = ok(&s.app, "/git/rich").await;
    assert!(res.body.contains(r#"href="/git/rich/tree/main/src""#));
    assert!(
        res.body
            .contains(r#"href="/git/rich/blob/main/docs/guide.md""#)
    );
    let res = fetch(&s.app, "/git/forks/nested.git", None).await;
    assert_eq!(res.header(header::LOCATION), "/git/forks/nested");
    ok(&s.app, "/git/rich/blob/main/src/main.rs").await;
}

#[tokio::test]
async fn every_referenced_icon_exists() {
    let s = site("");
    let res = ok(&s.app, "/rich").await;
    let sprite_url = res
        .body
        .split('"')
        .find(|s| s.contains("icons/sprite.svg#"))
        .unwrap()
        .split('#')
        .next()
        .unwrap()
        .to_string();
    let sprite = ok(&s.app, &sprite_url).await.body;
    for id in res
        .body
        .split("sprite.svg#")
        .skip(1)
        .map(|s| s.split('"').next().unwrap())
    {
        assert!(
            sprite.contains(&format!("id=\"{id}\"")),
            "missing icon {id}"
        );
    }
}
