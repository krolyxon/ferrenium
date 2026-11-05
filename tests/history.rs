//! History pages (log, commit, patch) checked against the git CLI.

mod common;

use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::{Work, bare, git};
use ferrenium::config::Config;
use ferrenium::server;
use ferrenium::state::{AppState, Overrides};
use tower::ServiceExt;

/// Commit ids in [`history_repo`], oldest first.
struct History {
    bare: PathBuf,
    root: String,
    tweak: String,
    rename: String,
    misc: String,
    merge: String,
    whitespace: String,
    head: String,
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

/// Root commit; a tweak; a rename with an edit; binary + mode change +
/// deletion; a merge of a side branch; a whitespace-only change; a
/// `.mailmap`. Authors vary so mailmap and grep have something to do.
fn history_repo(root: &Path) -> History {
    let bare = bare(root, "hist.git", &[]);
    let work = Work::clone_of(&bare);
    let w = work.path();
    let write = |name: &str, data: &[u8]| {
        let path = w.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    let commit = |args: &[&str]| {
        git(w, &["add", "-A"]);
        let mut all = vec!["commit", "-q"];
        all.extend_from_slice(args);
        git(w, &all);
        git(w, &["rev-parse", "HEAD"])
    };

    write("a.txt", lines(20).as_bytes());
    write("src/lib.rs", b"pub fn one() -> u32 {\n    1\n}\n");
    write("gone.txt", b"bye\n");
    write("run.sh", b"#!/bin/sh\necho hi\n");
    let root_commit = commit(&["-m", "Initial import"]);

    write(
        "a.txt",
        lines(20).replace("line 5\n", "line five\n").as_bytes(),
    );
    let tweak = commit(&[
        "-m",
        "Tweak a.txt\n\nThe body mentions a Needle.",
        "--author",
        "Bob <bob@old.example>",
    ]);

    git(w, &["mv", "a.txt", "b.txt"]);
    write(
        "b.txt",
        lines(20)
            .replace("line 5\n", "line five\n")
            .replace("line 20\n", "line twenty\n")
            .as_bytes(),
    );
    let rename = commit(&["-m", "Rename a to b"]);

    write("blob.bin", b"\0\x01\x02\x03binary");
    std::fs::remove_file(w.join("gone.txt")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(w.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let misc = commit(&["-m", "Binary, mode and deletion"]);

    git(w, &["checkout", "-q", "-b", "side"]);
    write("side.txt", b"from the side\n");
    commit(&["-m", "Side work"]);
    git(w, &["checkout", "-q", "main"]);
    write(
        "src/lib.rs",
        b"pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n",
    );
    commit(&["-m", "Add two"]);
    git(w, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
    let merge = git(w, &["rev-parse", "HEAD"]);

    write(
        "src/lib.rs",
        b"pub fn one() -> u32 {\n\t1\n}\n\npub fn two() -> u32 {\n    2\n}\n",
    );
    let whitespace = commit(&["-m", "Tabs"]);

    write(
        ".mailmap",
        b"Ada Lovelace <ada@example.com>\nRobert <bob@new.example> <bob@old.example>\n",
    );
    let head = commit(&["-m", "Add mailmap"]);
    git(w, &["push", "-q", "origin", "main", "side"]);
    History {
        bare,
        root: root_commit,
        tweak,
        rename,
        misc,
        merge,
        whitespace,
        head,
    }
}

struct Site {
    _root: tempfile::TempDir,
    app: axum::Router,
    h: History,
}

fn site(extra: &str) -> Site {
    let root = tempfile::tempdir().unwrap();
    let h = history_repo(root.path());
    let toml = format!(
        "[repos]\nscan_path = {:?}\n[watch]\nenabled = false\n{extra}",
        root.path().to_str().unwrap()
    );
    let mut config = Config::parse(&toml).expect("parse");
    config.finish().expect("valid");
    let state = AppState::new(config, Overrides::default());
    let config = state.config.load_full();
    state.index.full_scan(&config);
    Site {
        app: server::app(state),
        h,
        _root: root,
    }
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
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

async fn ok(app: &axum::Router, uri: &str) -> String {
    let res = fetch(app, uri, None).await;
    assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.body);
    assert!(!res.body.contains("style=\""), "style attribute in {uri}");
    res.body
}

/// Commit ids linked from log rows, in page order.
fn log_ids(html: &str) -> Vec<String> {
    html.split(r#"class="sha log-sha" href="/hist/commit/"#)
        .skip(1)
        .map(|rest| rest[..40].to_string())
        .collect()
}

fn next_link(html: &str, rel: &str) -> Option<String> {
    let end = html.find(&format!(r#"" rel="{rel}""#))?;
    let start = html[..end].rfind("href=\"")? + 6;
    Some(html[start..end].replace("&#38;", "&").replace("&amp;", "&"))
}

/// Follow `Older` links from `uri` and collect every commit id.
async fn all_pages(app: &axum::Router, uri: &str) -> (Vec<String>, usize) {
    let (mut ids, mut pages, mut url) = (Vec::new(), 0, uri.to_string());
    loop {
        let html = ok(app, &url).await;
        pages += 1;
        ids.extend(log_ids(&html));
        match next_link(&html, "next") {
            Some(next) => url = next,
            None => return (ids, pages),
        }
    }
}

fn git_log(bare: &Path, args: &[&str]) -> Vec<String> {
    let mut all = vec!["log", "--format=%H"];
    all.extend_from_slice(args);
    git(bare, &all).lines().map(str::to_string).collect()
}

#[tokio::test]
async fn log_pages_match_git() {
    let s = site("[ui]\nlog_page = 3\n");
    let (ids, pages) = all_pages(&s.app, "/hist/log").await;
    assert_eq!(ids, git_log(&s.h.bare, &["main"]));
    assert_eq!(pages, ids.len().div_ceil(3));

    // Second page: "newer" goes back to the first.
    let first = ok(&s.app, "/hist/log/main").await;
    let older = next_link(&first, "next").unwrap();
    let second = ok(&s.app, &older).await;
    assert_eq!(next_link(&second, "prev").unwrap(), "/hist/log/main");
    let third = ok(&s.app, &next_link(&second, "next").unwrap()).await;
    assert_eq!(next_link(&third, "prev").unwrap(), older);

    // Day groups and nav.
    assert!(first.contains(r#"class="label section-label""#));
    assert!(
        first.contains(r#"class="label active" href="/hist/log/main" aria-current="page">Log<"#),
        "{first}"
    );
    // Mailmap applies to authors.
    assert!(first.contains("Ada Lovelace"), "{first}");
    assert!(!first.contains(">Ada ·"));
}

#[tokio::test]
async fn path_filtered_log_matches_git() {
    let s = site("[ui]\nlog_page = 2\n");
    for path in [
        "src/lib.rs",
        "src",
        "b.txt",
        "a.txt",
        "gone.txt",
        "side.txt",
        "run.sh",
    ] {
        let (ids, _) = all_pages(&s.app, &format!("/hist/log/main/{path}")).await;
        assert_eq!(ids, git_log(&s.h.bare, &["main", "--", path]), "{path}");
    }
    let html = ok(&s.app, "/hist/log/main/src/lib.rs").await;
    assert!(
        html.contains(r#"<span class="crumb-here">lib.rs</span>"#),
        "{html}"
    );
    let empty = ok(&s.app, "/hist/log/main/nope").await;
    assert!(empty.contains("[No commits]"));
}

#[tokio::test]
async fn grep_filters_message_and_author() {
    let s = site("");
    let html = ok(&s.app, "/hist/log?q=needle").await;
    assert_eq!(log_ids(&html), vec![s.h.tweak.clone()]);
    assert!(html.contains(r#"value="needle""#));
    let html = ok(&s.app, "/hist/log/main?q=BOB%40OLD").await;
    assert_eq!(log_ids(&html), vec![s.h.tweak.clone()]);
    let html = ok(&s.app, "/hist/log?q=zzz-nothing").await;
    assert!(html.contains("[No commits]"));

    // The scan limit stops a search and offers to continue.
    let s = site("[limits]\nlog_scan = 2\n");
    let html = ok(&s.app, "/hist/log?q=initial").await;
    assert!(log_ids(&html).is_empty());
    assert!(html.contains("[No matches in 2 commits]"), "{html}");
    let (ids, _) = all_pages(&s.app, "/hist/log?q=initial").await;
    assert_eq!(ids, vec![s.h.root.clone()]);
}

#[tokio::test]
async fn bad_cursors_are_not_found() {
    let s = site("");
    let res = fetch(&s.app, "/hist/log?after=xyz", None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = fetch(&s.app, &format!("/hist/log?after={}", "0".repeat(40)), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(res.body.contains("COMMIT NOT IN THIS HISTORY"));
}

/// `+added −removed` per file from `git diff --numstat`, as the commit page
/// shows them.
fn numstat(bare: &Path, commit: &str) -> Vec<(String, String)> {
    let out = git(
        bare,
        &["diff", "--numstat", "-M", &format!("{commit}^"), commit],
    );
    out.lines()
        .map(|l| {
            let mut parts = l.split('\t');
            let (a, r) = (parts.next().unwrap(), parts.next().unwrap());
            (a.to_string(), r.to_string())
        })
        .collect()
}

#[tokio::test]
async fn commit_page_shows_metadata_and_diffstat() {
    let s = site("");
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.tweak)).await;
    assert!(
        html.contains(r#"<h2 class="commit-subject">Tweak a.txt</h2>"#),
        "{html}"
    );
    assert!(html.contains("The body mentions a Needle."));
    // Mailmap: Bob's old address maps to Robert.
    assert!(
        html.contains(r#"title="bob@new.example">Robert<"#),
        "{html}"
    );
    assert!(html.contains(&format!(r#"href="/hist/commit/{}""#, s.h.root)));
    assert!(html.contains(r#"<span class="add">+1</span> <span class="del">&minus;1</span>"#));
    assert!(
        html.contains(
            r#"<tr class="del"><td class="ln">5</td><td class="ln"></td><td class="sign">"#
        )
    );
    assert!(html.contains(r#"<tr class="add"><td class="ln"></td><td class="ln">5</td><td class="sign">+</td><td class="lc">line five</td>"#), "{html}");
    assert!(html.contains("@@ -2,7 +2,7 @@ line 1"));

    // Rename with an edit: stats match git, the old name is shown.
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.rename)).await;
    let stats = numstat(&s.h.bare, &s.h.rename);
    assert_eq!(stats, vec![("1".into(), "1".into())]);
    assert!(html.contains(r#"title="Renamed">R<"#));
    assert!(html.contains("a.txt &rarr; </span>b.txt"), "{html}");

    // Binary, mode change and deletion.
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.misc)).await;
    assert!(html.contains("[Binary file]"));
    assert!(html.contains("Mode 100644 &rarr; 100755"), "{html}");
    assert!(html.contains(r#"title="Deleted">D<"#));
    // A deleted file links to its last version.
    let parent = git(&s.h.bare, &["rev-parse", &format!("{}^", s.h.misc)]);
    assert!(html.contains(&format!(r#"href="/hist/blob/{parent}/gone.txt""#)));

    // Root commit: everything added.
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.root)).await;
    assert!(html.contains("<dd>None</dd>"), "{html}");
    assert!(html.contains("4 files changed"));

    // Merge: diffed against the first parent, both parents listed.
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.merge)).await;
    assert!(html.contains(">Parents<"));
    assert!(html.contains("side.txt"));
    assert!(!html.contains("pub fn two"), "first-parent diff: {html}");
}

#[tokio::test]
async fn whitespace_and_context_options() {
    let s = site("");
    let url = format!("/hist/commit/{}", s.h.whitespace);
    let html = ok(&s.app, &url).await;
    assert!(html.contains(r#"<span class="add">+1</span> <span class="del">&minus;1</span>"#));
    let html = ok(&s.app, &format!("{url}?ws=ignore")).await;
    assert!(html.contains("[No content changes]"), "{html}");
    assert!(html.contains(&format!(
        r#"href="{url}?ws=ignore" aria-current="true">Ignore<"#
    )));

    let html = ok(&s.app, &format!("{url}?context=0")).await;
    assert!(html.contains("@@ -2 +2 @@ pub fn one"), "{html}");
    assert!(html.contains("Context 0"));
    let html = ok(&s.app, &format!("{url}?context=999")).await;
    assert!(html.contains("Context 50"));
}

#[tokio::test]
async fn patch_matches_git() {
    let s = site("");
    for commit in [
        &s.h.root,
        &s.h.tweak,
        &s.h.rename,
        &s.h.misc,
        &s.h.merge,
        &s.h.head,
    ] {
        let res = fetch(&s.app, &format!("/hist/commit/{commit}.patch"), None).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(
            res.headers[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert!(
            res.headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("sandbox")
        );
        assert!(
            res.body
                .starts_with(&format!("From {commit} Mon Sep 17 00:00:00 2001\n"))
        );
        let ours: String = res
            .body
            .split_inclusive('\n')
            .skip_while(|l| !l.starts_with("diff --git"))
            .take_while(|l| *l != "-- \n")
            .collect();
        let base = if commit == &s.h.root {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
        } else {
            format!("{commit}^")
        };
        let theirs = std::process::Command::new("git")
            .args([
                "-c",
                "core.abbrev=7",
                "diff",
                "--no-color",
                "-M",
                "--abbrev=7",
                &base,
                commit,
            ])
            .current_dir(&s.h.bare)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
            .stdout;
        assert_eq!(ours, String::from_utf8(theirs).unwrap(), "{commit}");
    }
    let res = fetch(&s.app, &format!("/hist/commit/{}.patch", s.h.tweak), None).await;
    assert!(
        res.body.contains("From: Bob <bob@old.example>\n"),
        "patches keep the recorded author"
    );
    assert!(
        res.body
            .contains("Subject: [PATCH] Tweak a.txt\n\nThe body mentions a Needle.\n---\n")
    );
    assert!(
        res.body
            .contains(" 1 file changed, 1 insertion(+), 1 deletion(-)\n")
    );
}

#[tokio::test]
async fn diff_limits() {
    let s = site("[limits]\ndiff_file_lines = 1\n");
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.tweak)).await;
    assert!(html.contains("[Diff too large]"), "{html}");
    assert!(
        html.contains(r#"<span class="add">+1</span>"#),
        "stats still shown"
    );

    let s = site("[limits]\ndiff_files = 2\n");
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.root)).await;
    assert!(html.contains("[Diff too large: 4 files]"), "{html}");
    assert!(html.contains("src/lib.rs"));

    // The per-commit line budget.
    let s = site("[limits]\ndiff_lines = 1\n");
    let html = ok(&s.app, &format!("/hist/commit/{}", s.h.tweak)).await;
    assert!(html.contains("[Diff too large]"), "{html}");
}

#[tokio::test]
async fn caching_and_resolution() {
    let s = site("");
    // Full ids are immutable; refs revalidate.
    let res = fetch(&s.app, &format!("/hist/commit/{}", s.h.head), None).await;
    assert_eq!(res.headers[header::CACHE_CONTROL], "public, max-age=86400");
    let etag = res.headers[header::ETAG].to_str().unwrap().to_string();
    let res = fetch(&s.app, &format!("/hist/commit/{}", s.h.head), Some(&etag)).await;
    assert_eq!(res.status, StatusCode::NOT_MODIFIED);

    let res = fetch(&s.app, "/hist/log/main", None).await;
    assert_eq!(res.headers[header::CACHE_CONTROL], "no-cache");
    let etag = res.headers[header::ETAG].to_str().unwrap().to_string();
    let res = fetch(&s.app, "/hist/log/main", Some(&etag)).await;
    assert_eq!(res.status, StatusCode::NOT_MODIFIED);
    // A different query is a different page.
    let res = fetch(&s.app, "/hist/log/main?q=x", Some(&etag)).await;
    assert_eq!(res.status, StatusCode::OK);

    // Commits by branch name and short id.
    let html = ok(&s.app, "/hist/commit/main").await;
    assert!(html.contains("Add mailmap"));
    let html = ok(&s.app, &format!("/hist/commit/{}", &s.h.tweak[..8])).await;
    assert!(html.contains("Tweak a.txt"));
    let res = fetch(&s.app, "/hist/commit/nope", None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = fetch(&s.app, "/hist/commit", None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Links into history from other pages.
    let html = ok(&s.app, "/hist").await;
    assert!(html.contains(r#"<a href="/hist/log">"#), "{html}");
    assert!(html.contains(&format!(r#"href="/hist/commit/{}""#, s.h.head)));
    let html = ok(&s.app, "/hist/blob/main/src/lib.rs").await;
    assert!(html.contains(r#"href="/hist/log/main/src/lib.rs">History<"#));
    let html = ok(&s.app, "/hist/tree/main/src").await;
    assert!(html.contains(r#"href="/hist/log/main/src">History<"#));
}
