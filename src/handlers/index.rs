//! `/` — repository index.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::{Layout, render};
use crate::config::IndexSort;
use crate::index::Repo;
use crate::render::{encode_path, time};
use crate::state::AppState;

/// Repos pushed within this window get the activity dot.
const FRESH_SECS: i64 = 24 * 3600;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: String,
}

pub struct Row {
    pub name: String,
    pub url: String,
    pub description: Option<String>,
    pub age: Option<String>,
    pub iso: Option<String>,
    pub fresh: bool,
    /// Main language, once the background statistics have it.
    pub language: Option<&'static str>,
    /// Lowercased text the client-side filter matches against.
    pub search: String,
}

pub struct Section {
    pub name: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexPage {
    pub layout: Layout,
    pub description: String,
    /// Visible repositories in total.
    pub total: usize,
    /// Repositories matching the filter.
    pub shown: usize,
    pub q: String,
    pub by_activity: bool,
    /// `?sort=` value to keep when filtering; `None` for the default order.
    pub sort_param: Option<&'static str>,
    pub sort_name_url: String,
    pub sort_activity_url: String,
    pub sections: Vec<Section>,
}

pub async fn index(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IndexQuery>,
) -> Response {
    let config = state.config.load();
    let index = state.index.load();
    let default_sort = config.ui.index_sort;
    let sort = match query.sort.as_str() {
        "activity" => IndexSort::Activity,
        "name" => IndexSort::Name,
        _ => default_sort,
    };
    let by_activity = sort == IndexSort::Activity;
    let q = query.q.trim().to_string();
    let terms: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    let now = time::now();
    let base = &config.server.base_path;

    let mut repos: Vec<&Arc<Repo>> = index
        .repos
        .values()
        .filter(|repo| {
            let text = search_text(repo);
            terms.iter().all(|t| text.contains(t.as_str()))
        })
        .collect();
    let shown = repos.len();

    if by_activity {
        repos.sort_by(|a, b| {
            b.meta
                .last_activity
                .cmp(&a.meta.last_activity)
                .then_with(|| a.name.cmp(&b.name))
        });
    } else {
        repos.sort_by_cached_key(|r| {
            (
                r.meta.section.clone().map(|s| s.to_lowercase()),
                r.name.to_lowercase(),
            )
        });
    }

    let row = |repo: &Arc<Repo>| Row {
        name: repo.name.clone(),
        url: format!("{base}{}", encode_path(&repo.name)),
        description: repo.meta.description.clone(),
        age: repo.meta.last_activity.map(|t| time::relative(t, now)),
        iso: repo.meta.last_activity.map(time::iso),
        fresh: repo
            .meta
            .last_activity
            .is_some_and(|t| now - t < FRESH_SECS),
        language: state.warmer.primary(&repo.name),
        search: search_text(repo),
    };

    let mut sections: Vec<Section> = Vec::new();
    for repo in repos {
        // Sections only apply to the name ordering; activity is one list.
        let section = if by_activity {
            None
        } else {
            repo.meta.section.clone()
        };
        match sections.last_mut() {
            Some(last) if last.name == section => last.rows.push(row(repo)),
            _ => sections.push(Section {
                name: section,
                rows: vec![row(repo)],
            }),
        }
    }

    // The default order needs no `sort` parameter.
    let sort_url = |sort: IndexSort| {
        let mut params = Vec::new();
        if sort != default_sort {
            params.push(format!("sort={}", sort.as_str()));
        }
        if !q.is_empty() {
            params.push(format!("q={}", encode_path(&q)));
        }
        if params.is_empty() {
            base.clone()
        } else {
            format!("{base}?{}", params.join("&"))
        }
    };
    let page = IndexPage {
        layout: Layout::new(&state, &config, None),
        description: config.site.description.clone(),
        total: index.repos.len(),
        shown,
        sort_param: (sort != default_sort).then(|| sort.as_str()),
        sort_name_url: sort_url(IndexSort::Name),
        sort_activity_url: sort_url(IndexSort::Activity),
        q,
        by_activity,
        sections,
    };
    render(StatusCode::OK, &page)
}

fn search_text(repo: &Repo) -> String {
    let mut text = repo.name.to_lowercase();
    if let Some(d) = &repo.meta.description {
        text.push(' ');
        text.push_str(&d.to_lowercase());
    }
    text
}
