//! `/{repo}` — root tree, README and a rail with stats, clone URLs and the
//! latest commit.

use askama::Template;
use axum::http::StatusCode;
use axum::response::Response;

use super::repo::{Active, Ctx, RepoNav, set_cache_headers, short};
use super::tree::{Readme, TreeRow, listing, readme, rows};
use super::{Layout, render};
use crate::cache::Key;
use crate::git::refs::{RefKind, Rev};
use crate::git::{self, CommitInfo};
use crate::render::time;

pub struct Stat {
    pub label: &'static str,
    pub value: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

pub struct CloneUrl {
    pub label: String,
    pub url: String,
}

pub struct Latest {
    pub subject: String,
    pub author: String,
    pub age: String,
    pub iso: String,
    pub id: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "summary.html")]
pub struct SummaryPage {
    pub layout: Layout,
    pub nav: RepoNav,
    pub name: String,
    pub description: Option<String>,
    pub stats: Vec<Stat>,
    pub clone_urls: Vec<CloneUrl>,
    pub latest: Option<Latest>,
    /// `None` for an empty repository.
    pub rows: Option<Vec<TreeRow>>,
    pub tree_url: String,
    pub readme: Option<Readme>,
}

pub fn page(ctx: &Ctx) -> Response {
    let meta = &ctx.repo.meta;
    let rev = meta
        .default_branch
        .clone()
        .zip(meta.head)
        .map(|(name, commit)| Rev {
            name,
            kind: RefKind::Branch,
            commit,
        });
    let etag = ctx.etag(("summary", rev.as_ref().map(|r| r.commit)));
    if let Some(res) = ctx.not_modified(&etag, false) {
        return res;
    }
    let mut page = SummaryPage {
        layout: Layout::new(ctx.state, ctx.config, Some(&ctx.repo.name)),
        nav: ctx.nav(Active::Summary, rev.as_ref(), |name| {
            ctx.url("tree", name, b"")
        }),
        name: ctx.repo.name.clone(),
        description: meta.description.clone(),
        stats: Vec::new(),
        clone_urls: clone_urls(ctx),
        latest: None,
        rows: None,
        tree_url: String::new(),
        readme: None,
    };

    if let Some(rev) = &rev {
        let result = (|| -> anyhow::Result<()> {
            let commits = ctx.state.cache.count(Key::CommitCount(rev.commit), || {
                git::commit_count(&ctx.git, rev.commit)
            })?;
            let info = git::commit_info(&ctx.git, rev.commit)?;
            page.stats = stats(ctx, Some(commits));
            page.latest = Some(latest(ctx, &info));
            let root = git::tree::lookup(&ctx.git, rev.commit, b"")?;
            if let Some(git::tree::PathTarget::Tree(tree_id)) = root {
                page.rows = Some(rows(ctx, rev, b"", &listing(ctx, tree_id)?));
            }
            page.tree_url = ctx.url("tree", &rev.name, b"");
            page.readme = meta.readme.as_ref().and_then(|r| {
                let (dir, name) = r.path.rsplit_once('/').unwrap_or(("", &r.path));
                readme(ctx, rev, dir.as_bytes(), name, r.oid)
            });
            Ok(())
        })();
        if let Err(e) = result {
            return ctx.internal_error(e);
        }
    } else {
        page.stats = stats(ctx, None);
    }

    let mut res = render(StatusCode::OK, &page);
    set_cache_headers(&mut res, &etag, false);
    res
}

fn stats(ctx: &Ctx, commits: Option<u64>) -> Vec<Stat> {
    let meta = &ctx.repo.meta;
    let mut stats = vec![
        Stat {
            label: "Commits",
            value: commits.unwrap_or(0).to_string(),
            title: None,
            url: commits.map(|_| format!("{}/log", ctx.repo_url())),
        },
        Stat {
            label: "Branches",
            value: meta.branches.to_string(),
            title: None,
            url: None,
        },
        Stat {
            label: "Tags",
            value: meta.tags.to_string(),
            title: None,
            url: None,
        },
    ];
    if let Some(t) = meta.last_activity {
        stats.push(Stat {
            label: "Updated",
            value: time::relative(t, time::now()),
            title: Some(time::iso(t)),
            url: None,
        });
    }
    if let Some(owner) = &meta.owner {
        stats.push(Stat {
            label: "Owner",
            value: owner.clone(),
            title: None,
            url: None,
        });
    }
    stats
}

fn latest(ctx: &Ctx, info: &CommitInfo) -> Latest {
    let id = info.id.to_string();
    Latest {
        subject: info.subject.clone(),
        author: ctx
            .mailmap()
            .resolve(&info.author_name, &info.author_email)
            .0,
        age: time::relative(info.committer_time, time::now()),
        iso: time::iso(info.committer_time),
        url: ctx.commit_url(&info.id),
        id: short(&id),
    }
}

/// `site.clone_urls` with `{name}` filled in, labeled by protocol.
fn clone_urls(ctx: &Ctx) -> Vec<CloneUrl> {
    ctx.config
        .site
        .clone_urls
        .iter()
        .map(|template| {
            let url = template.replace("{name}", &ctx.repo.name);
            let label = match url.split_once("://") {
                Some(("ssh", _)) => "SSH".to_string(),
                Some((scheme, _)) => scheme.to_ascii_uppercase(),
                // scp-like `user@host:path`
                None => "SSH".to_string(),
            };
            CloneUrl { label, url }
        })
        .collect()
}
