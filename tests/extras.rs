//! Blame, archives, the Atom feed, the file finder, language statistics,
//! robots.txt and HTTPS clones, checked against the git CLI.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use common::{Work, bare, git};
use ferrenium::config::Config;
use ferrenium::index::languages;
use ferrenium::server;
use ferrenium::state::{AppState, Overrides};
use tower::ServiceExt;

struct Fixture {
    bare: PathBuf,
    head: String,
}

/// Several authors editing one file, a rename, vendored/generated/doc
/// files, a `.gitattributes` and a tag.
fn fixture(root: &Path) -> Fixture {
    let bare = bare(root, "ext.git", &[]);
    let work = Work::clone_of(&bare);
    let w = work.path();
    let write = |name: &str, data: &str| {
        let path = w.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    // A day apart, so blame ages differ.
    let day = std::cell::Cell::new(0);
    let commit = |message: &str, author: &str| {
        git(w, &["add", "-A"]);
        day.set(day.get() + 1);
        let date = format!("{} +0000", 1_700_000_000 + day.get() * 86_400);
        let status = Command::new("git")
            .args(["commit", "-q", "-m", message, "--author", author])
            .current_dir(w)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .status()
            .unwrap();
        assert!(status.success());
    };
    write("src/old.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    write("README.md", "# Ext\n");
    commit("Start", "Ada <ada@example.com>");
    write(
        "src/old.rs",
        "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n",
    );
    commit("Edit b, add d", "Bob <bob@example.com>");
    git(w, &["mv", "src/old.rs", "src/main.rs"]);
    commit("Rename", "Ada <ada@example.com>");
    write(
        "src/main.rs",
        "// top\nfn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n",
    );
    write("run.sh", "#!/bin/sh\necho run\n");
    write("vendor/lib.js", &"var x = 1;\n".repeat(500));
    write("docs/conf.py", &"x = 1\n".repeat(500));
    write("app.min.js", &"var y=2;".repeat(500));
    write("extra/big.c", &"int x;\n".repeat(500));
    write(".gitattributes", "extra/** linguist-vendored\n");
    commit("Add the rest", "Cy <cy@example.com>");
    git(w, &["tag", "v1.0"]);
    work.push("main");
    git(w, &["push", "-q", "origin", "--tags"]);
    let head = git(w, &["rev-parse", "HEAD"]);
    Fixture { bare, head }
}

struct Site {
    _root: tempfile::TempDir,
    state: Arc<AppState>,
    app: axum::Router,
    f: Fixture,
}

/// `repos` is extra lines for the `[repos]` table.
fn site(repos: &str) -> Site {
    site_with(repos, "")
}

/// Like [`site`], with `tables` appended to the config.
fn site_with(repos: &str, tables: &str) -> Site {
    let root = tempfile::tempdir().unwrap();
    let f = fixture(root.path());
    let toml = format!(
        "[repos]\nscan_path = {:?}\n{repos}\n[watch]\nenabled = false\n\
         [server]\nbase_url = \"https://git.example.com\"\n{tables}\n",
        root.path().to_str().unwrap()
    );
    let mut config = Config::parse(&toml).expect("parse");
    config.finish().expect("valid");
    let state = AppState::new(config, Overrides::default());
    let config = state.config.load_full();
    state.index.full_scan(&config);
    Site {
        app: server::app(state.clone()),
        state,
        f,
        _root: root,
    }
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Res {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn request(app: &axum::Router, method: Method, uri: &str) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Res {
        status,
        headers,
        body,
    }
}

async fn get(app: &axum::Router, uri: &str) -> Res {
    request(app, Method::GET, uri).await
}

async fn ok(app: &axum::Router, uri: &str) -> String {
    let res = get(app, uri).await;
    assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.text());
    res.text()
}

/// Commit of every line, from the page's hunk heads.
fn blamed_lines(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in html.split(r#"class="blame-meta "#).skip(1) {
        let rows: usize = part
            .split(r#"rowspan=""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let id = part.split("/commit/").nth(1).unwrap()[..40].to_string();
        out.extend(std::iter::repeat_n(id, rows));
    }
    out
}

#[tokio::test]
async fn blame_matches_git() {
    let s = site("");
    for path in ["src/main.rs", "run.sh", "README.md"] {
        let html = ok(&s.app, &format!("/ext/blame/main/{path}")).await;
        let porcelain = git(&s.f.bare, &["blame", "--porcelain", "main", "--", path]);
        let expected: Vec<String> = porcelain
            .lines()
            .filter(|l| {
                l.len() > 41
                    && l.as_bytes()[40] == b' '
                    && l[..40].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|l| l[..40].to_string())
            .collect();
        assert_eq!(blamed_lines(&html), expected, "{path}");
    }
    let html = ok(&s.app, "/ext/blame/main/src/main.rs").await;
    // Hunk heads: author, age bar (newest is full strength), links.
    assert!(html.contains("blame-meta age-8"), "{html}");
    assert!(html.contains("blame-meta age-0"));
    assert!(html.contains(">Bob<"));
    assert!(html.contains("5 lines · 3 commits"), "{html}");
    assert!(html.contains(r#"href="/ext/blob/main/src/main.rs">File<"#));
    // The blob page links here; directories redirect; binaries don't blame.
    let blob = ok(&s.app, "/ext/blob/main/src/main.rs").await;
    assert!(blob.contains(r#"href="/ext/blame/main/src/main.rs">Blame<"#));
    let res = get(&s.app, "/ext/blame/main/src").await;
    assert_eq!(res.status, StatusCode::TEMPORARY_REDIRECT);
    let res = get(&s.app, "/ext/blame/main/nope").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

fn tar_listing(data: &[u8]) -> String {
    let mut child = Command::new("tar")
        .args(["tzf", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    // git also lists directories; only files matter.
    let mut lines: Vec<&str> = std::str::from_utf8(&out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.ends_with('/'))
        .collect();
    lines.sort();
    lines.join("\n")
}

#[tokio::test]
async fn archives_match_git_archive() {
    let s = site("");
    let res = get(&s.app, "/ext/archive/v1.0.tar.gz").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.headers[header::CONTENT_TYPE], "application/gzip");
    assert!(
        res.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("filename=\"ext-v1.0.tar.gz\"")
    );
    assert!(res.headers.get(header::CONTENT_ENCODING).is_none());
    let ours = tar_listing(&res.body);
    let theirs = Command::new("git")
        .args(["archive", "--format=tar.gz", "--prefix=ext-v1.0/", "v1.0"])
        .current_dir(&s.f.bare)
        .output()
        .unwrap()
        .stdout;
    assert_eq!(ours, tar_listing(&theirs));
    assert!(ours.contains("ext-v1.0/src/main.rs"));

    let res = get(&s.app, &format!("/ext/archive/{}.zip", s.f.head)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.headers[header::CONTENT_TYPE], "application/zip");
    assert_eq!(res.headers[header::CACHE_CONTROL], "public, max-age=86400");
    assert_eq!(&res.body[..2], b"PK");

    for uri in [
        "/ext/archive/nope.zip",
        "/ext/archive/main.rar",
        "/ext/archive/main",
    ] {
        assert_eq!(
            get(&s.app, uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    let s = site("snapshots = [\"tar.gz\"]");
    assert_eq!(
        get(&s.app, "/ext/archive/main.zip").await.status,
        StatusCode::NOT_FOUND
    );
    let refs = ok(&s.app, "/ext/refs").await;
    assert!(
        refs.contains(r#"href="/ext/archive/v1.0.tar.gz""#),
        "{refs}"
    );
    assert!(!refs.contains(".zip"));
}

#[tokio::test]
async fn atom_feed() {
    let s = site("");
    let res = get(&s.app, "/ext/atom").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.headers[header::CONTENT_TYPE],
        "application/atom+xml; charset=utf-8"
    );
    let xml = res.text();
    assert!(xml.starts_with("<?xml"));
    assert_eq!(xml.matches("<entry>").count(), 4);
    assert!(xml.contains("<id>https://git.example.com/ext/atom</id>"));
    assert!(xml.contains(&format!(
        r#"href="https://git.example.com/ext/commit/{}""#,
        s.f.head
    )));
    assert!(xml.contains("<name>Cy</name>"));
    assert_eq!(
        get(&s.app, "/ext/atom?h=nope").await.status,
        StatusCode::NOT_FOUND
    );
    // Advertised on the summary.
    let html = ok(&s.app, "/ext").await;
    assert!(html.contains(
        r#"<link rel="alternate" type="application/atom+xml" title="Commits" href="/ext/atom">"#
    ));
}

#[tokio::test]
async fn find_files() {
    let s = site("");
    let html = ok(&s.app, "/ext/find/main?q=MAIN").await;
    assert!(
        html.contains(r#"href="/ext/blob/main/src/main.rs""#),
        "{html}"
    );
    assert!(html.contains("1 of 8 files"), "{html}");
    let html = ok(&s.app, "/ext/find/main?q=zzz").await;
    assert!(html.contains("[No matches]"));
    let html = ok(&s.app, "/ext/find/main").await;
    assert!(html.contains("8 files"));
    assert!(html.contains(r#"aria-current="page">Find<"#));
}

#[tokio::test]
async fn language_statistics() {
    let s = site("");
    let repo = s.state.index.load().repos["ext"].clone();
    let handle = ferrenium::git::handle(&repo.gix);
    let tree = languages::root_tree(&handle, repo.meta.head.unwrap()).unwrap();
    let stats = languages::compute(&handle, tree).unwrap();
    let names: Vec<&str> = stats.languages.iter().map(|(n, _)| *n).collect();
    // Vendored, documentation, minified and attribute-vendored files, and
    // prose (README), don't count.
    assert_eq!(names, ["Rust", "Shell"], "{stats:?}");

    // The summary shows the bar once the warmer has run.
    let html = ok(&s.app, "/ext").await;
    assert!(!html.contains("lang-bar"));
    s.state
        .cache
        .languages(tree, || Ok::<_, std::convert::Infallible>(stats))
        .unwrap();
    let html = ok(&s.app, "/ext").await;
    assert!(html.contains(r#"class="lang-bar""#), "{html}");
    assert_eq!(
        html.matches(r#"<i class="o0"></i>"#).count()
            + html.matches(r#"<i class="o1"></i>"#).count(),
        40
    );
}

#[tokio::test]
async fn robots_and_methods() {
    let s = site("");
    let robots = ok(&s.app, "/robots.txt").await;
    assert!(robots.contains("Disallow: /*/blame/"));
    assert!(robots.contains("Disallow: /*/archive/"));
    assert!(robots.contains("Disallow: /*?q="));
    assert_eq!(
        request(&s.app, Method::POST, "/ext/tree/main").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        get(&s.app, "/ext/info/refs?service=git-receive-pack")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&s.app, "/ext/info/refs").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&s.app, Method::POST, "/ext.git/git-receive-pack")
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&s.app, "/ext.git/tree/main").await.status,
        StatusCode::NOT_FOUND
    );
    let s = site("https_clone = false");
    assert_eq!(
        get(&s.app, "/ext/info/refs?service=git-upload-pack")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn hardening() {
    let s = site_with(
        "",
        "[limits]\nheavy_parallel = 1\nheavy_wait_secs = 0\nmax_requests = 4",
    );

    // Error pages are styled and carry the security headers.
    for res in [
        request(&s.app, Method::POST, "/").await,
        request(&s.app, Method::DELETE, "/ext/tree/main").await,
    ] {
        assert_eq!(res.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers[header::ALLOW], "GET, HEAD");
        assert!(res.text().contains("[ERROR: METHOD NOT ALLOWED]"));
    }
    let res = get(&s.app, "/nope/nothing").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(res.text().contains("[ERROR: PAGE NOT FOUND]"));
    for name in [
        "content-security-policy",
        "x-content-type-options",
        "referrer-policy",
        "x-frame-options",
        "cross-origin-opener-policy",
    ] {
        assert!(res.headers.contains_key(name), "missing {name}");
    }

    // Heavy pages give up after `heavy_wait_secs`; others are unaffected.
    let held = s.state.heavy_permits.clone().try_acquire_owned().unwrap();
    let res = get(&s.app, "/ext/blame/main/src/main.rs").await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers[header::RETRY_AFTER], "10");
    assert_eq!(res.headers[header::CACHE_CONTROL], "no-store");
    assert!(res.text().contains("[ERROR: SERVER BUSY]"));
    assert_eq!(
        get(&s.app, "/ext/info/refs?service=git-upload-pack")
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    ok(&s.app, "/ext/tree/main").await;
    drop(held);
    ok(&s.app, "/ext/blame/main/src/main.rs").await;

    // Past `max_requests`, everything is refused at once.
    let held = s
        .state
        .request_permits
        .clone()
        .try_acquire_many_owned(4)
        .unwrap();
    assert_eq!(
        get(&s.app, "/").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(held);
    ok(&s.app, "/").await;

    // Oversized fetch negotiations never reach git.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/ext.git/git-upload-pack")
        .header(header::CONTENT_LENGTH, "1000000000")
        .body(Body::empty())
        .unwrap();
    let res = s.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_over_https() {
    let s = site("");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = s.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dest = tempfile::tempdir().unwrap();
    let head = s.f.head.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut results = Vec::new();
        for (url, dir, protocol) in [
            (format!("http://{addr}/ext.git"), "v2", "2"),
            (format!("http://{addr}/ext"), "v0", "0"),
        ] {
            let target = dest.path().join(dir);
            let status = Command::new("git")
                .args([
                    "-c",
                    &format!("protocol.version={protocol}"),
                    "clone",
                    "-q",
                    &url,
                ])
                .arg(&target)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .status()
                .unwrap();
            assert!(status.success(), "clone {url}");
            results.push(git(&target, &["rev-parse", "HEAD"]));
        }
        // Pushing is refused.
        let work = dest.path().join("v2");
        std::fs::write(work.join("new.txt"), "x").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "x"]);
        let push = Command::new("git")
            .args(["push", "-q", "origin", "main"])
            .current_dir(&work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!push.status.success());
        assert!(String::from_utf8_lossy(&push.stderr).contains("403"));
        results
    })
    .await
    .unwrap();
    assert_eq!(out, vec![head.clone(), head]);
}
