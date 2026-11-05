//! Discovery, metadata precedence, hiding, the index page and live updates.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use common::{Work, bare, git, repo_with};
use ferrenium::config::Config;
use ferrenium::index::Refresh;
use ferrenium::server;
use ferrenium::state::{AppState, Overrides};
use tower::ServiceExt;

/// `repos` holds extra `[repos]` keys; `extra` any further tables.
fn config(scan_path: &std::path::Path, repos: &str, extra: &str) -> Config {
    let toml = format!(
        "[repos]\nscan_path = {:?}\n{repos}\n[watch]\ndebounce_ms = 100\n{extra}",
        scan_path.to_str().unwrap()
    );
    let mut config = Config::parse(&toml).expect("parse");
    config.finish().expect("valid");
    config
}

fn state(config: Config) -> Arc<AppState> {
    let state = AppState::new(config, Overrides::default());
    let config = state.config.load_full();
    state.index.full_scan(&config);
    state
}

async fn page(state: &Arc<AppState>, uri: &str) -> String {
    let res = server::app(state.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    String::from_utf8(
        to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[test]
fn metadata_precedence_and_hiding() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();

    // description file only
    let (a, a_head) = repo_with(r, "alpha.git", &[("README.md", "# alpha\n")]);
    std::fs::write(a.join("description"), "From the description file\n").unwrap();
    // git config beats description file; gitweb.* is honored
    let (b, _) = repo_with(r, "forks/beta.git", &[("readme.txt", "beta\n")]);
    std::fs::write(b.join("description"), "ignored\n").unwrap();
    git(&b, &["config", "gitweb.description", "From gitweb"]);
    git(&b, &["config", "gitweb.owner", "bob"]);
    // ferrenium.* beats gitweb.*; TOML beats everything
    let (c, _) = repo_with(r, "gamma.git", &[("x.rs", "fn main() {}\n")]);
    git(
        &c,
        &["config", "ferrenium.description", "From ferrenium config"],
    );
    git(&c, &["config", "gitweb.description", "loses"]);
    git(&c, &["config", "ferrenium.section", "tools"]);
    // empty repo with git's placeholder description
    bare(r, "empty.git", &[]);
    // hidden three ways
    repo_with(r, "private/secret.git", &[("a", "a")]);
    let (h2, _) = repo_with(r, "byconfig.git", &[("a", "a")]);
    git(&h2, &["config", "ferrenium.hidden", "true"]);
    repo_with(r, "bytoml.git", &[("a", "a")]);

    let state = state(config(
        r,
        "hide = [\"private/*\"]\nsection_from_path = true",
        r#"
        [[repo]]
        name = "gamma"
        description = "From TOML"
        [[repo]]
        name = "bytoml"
        hidden = true
        [[repo]]
        name = "nonexistent"
        "#,
    ));
    let index = state.index.load();

    let names: Vec<_> = index.repos.keys().map(String::as_str).collect();
    assert_eq!(names, ["alpha", "empty", "forks/beta", "gamma"]);
    let hidden: Vec<_> = index.hidden.iter().map(String::as_str).collect();
    assert_eq!(hidden, ["byconfig", "bytoml", "private/secret"]);

    let alpha = &index.repos["alpha"].meta;
    assert_eq!(
        alpha.description.as_deref(),
        Some("From the description file")
    );
    assert_eq!(alpha.default_branch.as_deref(), Some("main"));
    assert_eq!(alpha.head.unwrap().to_string(), a_head);
    assert_eq!(alpha.branches, 1);
    assert_eq!(alpha.readme.as_ref().unwrap().path, "README.md");
    assert!(alpha.last_activity.is_some());
    assert_eq!(alpha.section, None);

    let beta = &index.repos["forks/beta"].meta;
    assert_eq!(beta.description.as_deref(), Some("From gitweb"));
    assert_eq!(beta.owner.as_deref(), Some("bob"));
    assert_eq!(beta.section.as_deref(), Some("forks")); // section_from_path
    assert_eq!(beta.readme.as_ref().unwrap().path, "readme.txt");

    let gamma = &index.repos["gamma"].meta;
    assert_eq!(gamma.description.as_deref(), Some("From TOML"));
    assert_eq!(gamma.section.as_deref(), Some("tools"));
    assert_eq!(gamma.readme, None);

    let empty = &index.repos["empty"].meta;
    assert_eq!(empty.description, None); // placeholder ignored
    assert_eq!(empty.head, None);
    assert_eq!(empty.default_branch.as_deref(), Some("main"));
    assert_eq!(empty.last_activity, None);

    assert!(
        index.warnings.iter().any(|w| w.contains("\"nonexistent\"")),
        "{:?}",
        index.warnings
    );
}

#[test]
fn reftable_repos_are_skipped_with_a_warning() {
    let root = tempfile::tempdir().unwrap();
    repo_with(root.path(), "normal.git", &[("a", "a")]);
    bare(root.path(), "rt.git", &["--ref-format=reftable"]);
    let state = state(config(root.path(), "", ""));
    let index = state.index.load();
    assert_eq!(index.repos.keys().collect::<Vec<_>>(), ["normal"]);
    assert!(
        index
            .warnings
            .iter()
            .any(|w| w.starts_with("rt:") && w.contains("reftable")),
        "{:?}",
        index.warnings
    );
}

#[test]
fn unchanged_repos_keep_their_generation() {
    let root = tempfile::tempdir().unwrap();
    let (a, _) = repo_with(root.path(), "a.git", &[("f", "1")]);
    repo_with(root.path(), "b.git", &[("f", "1")]);
    let state = state(config(root.path(), "", ""));
    let cfg = state.config.load_full();
    let before = state.index.load();

    let stats = state.index.full_scan(&cfg);
    assert!(!stats.any_change(), "{stats:?}");
    let after = state.index.load();
    assert!(Arc::ptr_eq(&before.repos["a"], &after.repos["a"]));

    let work = Work::clone_of(&a);
    work.commit("f", "2", "change");
    work.push("main");
    assert_eq!(state.index.refresh(&cfg, &a), Refresh::Updated("a".into()));
    let refreshed = state.index.load();
    assert!(refreshed.repos["a"].generation > after.repos["a"].generation);
    assert!(Arc::ptr_eq(&after.repos["b"], &refreshed.repos["b"]));
    assert_eq!(state.index.refresh(&cfg, &a), Refresh::Unchanged);
}

#[tokio::test]
async fn index_page_lists_sections_filter_and_sort() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let (a, _) = repo_with(r, "zeta.git", &[("f", "1")]);
    git(
        &a,
        &["config", "ferrenium.description", "Last <b>letter</b>"],
    );
    git(&a, &["config", "ferrenium.section", "greek"]);
    repo_with(r, "alpha.git", &[("f", "1")]);
    repo_with(r, "with space.git", &[("f", "1")]);
    bare(r, "empty.git", &[]);
    let state = state(config(r, "", ""));

    // Default order: by activity, one list.
    let default = page(&state, "/").await;
    assert!(!default.contains("[ greek ]"));
    assert!(!default.contains(r#"name="sort""#));

    let html = page(&state, "/?sort=name").await;
    assert!(html.contains("4 Repositories"));
    assert!(html.contains(r#"href="/alpha""#));
    assert!(html.contains(r#"href="/with%20space""#));
    assert!(html.contains("Last &#60;b&#62;letter&#60;/b&#62;") || html.contains("Last &lt;b&gt;"));
    assert!(html.contains("[ greek ]"));
    // Unsectioned repos come first, in name order.
    let pos = |s: &str| html.find(s).unwrap_or_else(|| panic!("{s} missing"));
    assert!(pos("/alpha\"") < pos("/empty\"") && pos("/empty\"") < pos("[ greek ]"));
    // Just pushed: fresh dot. Empty repo: no age.
    assert!(html.contains(r#"class="dot""#));
    assert!(html.contains(">Empty<"));

    let filtered = page(&state, "/?q=LETTER").await;
    assert!(filtered.contains(r#"href="/zeta""#));
    assert!(!filtered.contains(r#"href="/alpha""#));
    assert!(filtered.contains(r#"value="LETTER""#));

    let none = page(&state, "/?q=nothing-matches").await;
    assert!(none.contains("[No matches]"));
    assert!(!none.contains("data-filter-empty hidden"));

    assert!(html.contains(r#"name="sort" value="name""#));
    let by_activity = page(&state, "/?sort=activity").await;
    assert!(!by_activity.contains("[ greek ]"));
    assert!(!by_activity.contains(r#"name="sort""#));

    // `ui.index_sort = "name"` makes name order the default.
    let state = state_for_name_sort(r);
    let html = page(&state, "/").await;
    assert!(html.contains("[ greek ]"));
    assert!(!html.contains(r#"name="sort""#));
    assert!(html.contains(r#"href="/?sort=activity""#));
}

fn state_for_name_sort(root: &std::path::Path) -> Arc<AppState> {
    state(config(root, "", "[ui]\nindex_sort = \"name\"\n"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_updates_the_index_quickly() {
    let root = tempfile::tempdir().unwrap();
    let (repo, first) = repo_with(root.path(), "live.git", &[("f", "1")]);
    let state = state(config(root.path(), "", ""));
    tokio::spawn(ferrenium::index::watch::run(
        state.clone(),
        Default::default(),
    ));
    // Let the watcher install its watches.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let wait_for = |pred: &dyn Fn(&ferrenium::index::RepoIndex) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if pred(&state.index.load()) {
                return Instant::now();
            }
            assert!(Instant::now() < deadline, "index did not update in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // A push to an existing repo.
    let work = Work::clone_of(&repo);
    let second = work.commit("f", "2", "second");
    assert_ne!(first, second);
    let pushed = Instant::now();
    work.push("main");
    let seen =
        wait_for(&|idx| idx.repos["live"].meta.head.map(|h| h.to_string()) == Some(second.clone()));
    let latency = seen - pushed;
    assert!(latency < Duration::from_secs(1), "took {latency:?}");

    // A new branch counts too.
    git(
        work.path(),
        &["push", "-q", "origin", "main:refs/heads/feature/x"],
    );
    wait_for(&|idx| idx.repos["live"].meta.branches == 2);

    // A new repository appears, a deleted one disappears.
    repo_with(root.path(), "nested/new.git", &[("f", "1")]);
    wait_for(&|idx| idx.repos.contains_key("nested/new"));
    std::fs::remove_dir_all(&repo).unwrap();
    wait_for(&|idx| !idx.repos.contains_key("live"));

    // Hiding via git config takes effect without a restart.
    let new_dir = root.path().join("nested/new.git");
    git(&new_dir, &["config", "ferrenium.hidden", "true"]);
    wait_for(&|idx| idx.hidden.contains("nested/new"));
}
