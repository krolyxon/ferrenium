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
use crate::index::languages::{self, Stats};
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

/// The segmented language bar: 40 blocks, the top three languages at
/// full, 70% and 45% strength, everything else at 25%.
pub struct LangBar {
    /// One class per block (`o0` … `o3`).
    pub blocks: Vec<&'static str>,
    pub legend: Vec<LangEntry>,
}

pub struct LangEntry {
    pub name: String,
    pub percent: String,
    pub class: &'static str,
}

const BAR_BLOCKS: u64 = 40;
const BAR_CLASSES: [&str; 4] = ["o0", "o1", "o2", "o3"];

pub fn language_bar(stats: &Stats) -> Option<LangBar> {
    if stats.total == 0 {
        return None;
    }
    let mut segments: Vec<(String, u64, &'static str)> = stats
        .languages
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, (name, bytes))| (name.to_string(), *bytes, BAR_CLASSES[i]))
        .collect();
    let rest: u64 = stats.languages.iter().skip(3).map(|(_, n)| n).sum();
    if rest > 0 {
        segments.push(("Other".into(), rest, BAR_CLASSES[3]));
    }
    // Largest remainder, so the blocks always add up to 40.
    let mut counts: Vec<(u64, u64, usize)> = segments
        .iter()
        .enumerate()
        .map(|(i, (_, bytes, _))| {
            let exact = bytes * BAR_BLOCKS;
            (exact / stats.total, exact % stats.total, i)
        })
        .collect();
    let mut left = BAR_BLOCKS - counts.iter().map(|c| c.0).sum::<u64>();
    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by(|&a, &b| counts[b].1.cmp(&counts[a].1).then(a.cmp(&b)));
    for i in order {
        if left == 0 {
            break;
        }
        counts[i].0 += 1;
        left -= 1;
    }
    let blocks = counts
        .iter()
        .flat_map(|(n, _, i)| std::iter::repeat_n(segments[*i].2, *n as usize))
        .collect();
    let legend = segments
        .into_iter()
        .map(|(name, bytes, class)| LangEntry {
            name,
            percent: format!("{:.1}%", bytes as f64 * 100.0 / stats.total as f64),
            class,
        })
        .collect();
    Some(LangBar { blocks, legend })
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
    pub languages: Option<LangBar>,
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
    let tree = rev
        .as_ref()
        .and_then(|r| languages::root_tree(&ctx.git, r.commit).ok());
    let lang_stats = tree.and_then(|t| languages::cached(&ctx.state.cache, t));
    if tree.is_some() && lang_stats.is_none() {
        ctx.state.warmer.wake.notify_one();
    }
    let etag = ctx.etag((
        "summary",
        rev.as_ref().map(|r| r.commit),
        lang_stats.is_some(),
    ));
    if let Some(res) = ctx.not_modified(&etag, false) {
        return res;
    }
    let mut page = SummaryPage {
        layout: Layout {
            feed: Some(ctx.feed_url()),
            ..Layout::new(ctx.state, ctx.config, Some(&ctx.repo.name))
        },
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
        languages: lang_stats.as_deref().and_then(language_bar),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_blocks_add_up() {
        let stats = Stats {
            languages: vec![
                ("Rust", 700),
                ("Shell", 200),
                ("CSS", 60),
                ("HTML", 30),
                ("Nix", 10),
            ],
            total: 1000,
        };
        let bar = language_bar(&stats).unwrap();
        assert_eq!(bar.blocks.len(), 40);
        let count = |c| bar.blocks.iter().filter(|b| **b == c).count();
        assert_eq!(
            (count("o0"), count("o1"), count("o2"), count("o3")),
            (28, 8, 2, 2)
        );
        let legend: Vec<_> = bar
            .legend
            .iter()
            .map(|l| (l.name.as_str(), l.percent.as_str()))
            .collect();
        assert_eq!(
            legend,
            [
                ("Rust", "70.0%"),
                ("Shell", "20.0%"),
                ("CSS", "6.0%"),
                ("Other", "4.0%")
            ]
        );
        assert!(language_bar(&Stats::default()).is_none());
    }
}
