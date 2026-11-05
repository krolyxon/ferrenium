//! One byte-weighted in-memory cache for rendered and computed data.
//!
//! Keys are content-addressed (blob/tree/commit ids) and never need
//! invalidation. Ref-dependent entries include the repository's
//! `generation`, so a refresh retires them naturally.

use std::sync::Arc;

use crate::git::blame::BlameHunk;
use crate::git::log::{LogPage, LogQuery};
use crate::git::refs::RefList;
use crate::git::tree::TreeEntry;
use crate::git::{Mailmap, ObjectId};
use crate::index::languages::Stats as LanguageStats;
use crate::render::diff::{CommitDiff, Options as DiffOptions};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// Highlighted lines of a blob with a given syntax.
    Highlight {
        blob: ObjectId,
        syntax: String,
    },
    /// Rendered Markdown; `context` is everything relative links depend on.
    Markdown {
        blob: ObjectId,
        context: String,
    },
    Tree(ObjectId),
    CommitCount(ObjectId),
    Refs {
        repo: String,
        generation: u64,
    },
    Log {
        tip: ObjectId,
        query: LogQuery,
    },
    Diff {
        commit: ObjectId,
        options: DiffOptions,
    },
    Mailmap(ObjectId),
    Blame {
        commit: ObjectId,
        path: Vec<u8>,
    },
    /// Every file path in a tree (the file finder).
    Paths(ObjectId),
    /// Language statistics of a tree.
    Languages(ObjectId),
}

#[derive(Clone)]
pub enum Value {
    Lines(Arc<Vec<String>>),
    Html(Arc<str>),
    Tree(Arc<Vec<TreeEntry>>),
    Count(u64),
    Refs(Arc<RefList>),
    Log(Arc<LogPage>),
    Diff(Arc<CommitDiff>),
    Mailmap(Arc<Mailmap>),
    Blame(Arc<Vec<BlameHunk>>),
    Paths(Arc<(Vec<String>, bool)>),
    Languages(Arc<LanguageStats>),
}

impl Value {
    fn weight(&self) -> usize {
        match self {
            Value::Lines(lines) => lines.iter().map(|l| l.len() + 24).sum(),
            Value::Html(html) => html.len(),
            Value::Tree(entries) => entries.iter().map(|e| e.name.len() + 64).sum(),
            Value::Count(_) => 8,
            Value::Refs(refs) => refs.weight(),
            Value::Log(page) => page.weight(),
            Value::Diff(diff) => diff.weight(),
            Value::Mailmap(map) => map.weight(),
            Value::Languages(stats) => stats.weight(),
            Value::Paths(paths) => paths.0.iter().map(|p| p.len() + 24).sum(),
            Value::Blame(hunks) => hunks
                .iter()
                .map(|h| 48 + h.source_path.as_ref().map_or(0, String::len))
                .sum(),
        }
    }
}

pub struct Cache {
    inner: moka::sync::Cache<Key, Value>,
}

impl Cache {
    pub fn new(size_mb: u64) -> Self {
        let inner = moka::sync::Cache::builder()
            .max_capacity(size_mb.saturating_mul(1024 * 1024))
            .weigher(|key: &Key, value: &Value| {
                let key = match key {
                    Key::Highlight { syntax, .. } => syntax.len(),
                    Key::Markdown { context, .. } => context.len(),
                    Key::Refs { repo, .. } => repo.len(),
                    Key::Blame { path, .. } => path.len(),
                    Key::Log { query, .. } => {
                        query.path.len() + query.grep.as_ref().map_or(0, String::len)
                    }
                    _ => 0,
                };
                u32::try_from(64 + key + value.weight()).unwrap_or(u32::MAX)
            })
            .build();
        Cache { inner }
    }

    /// Cached value for `key`, computing (and storing) it on a miss.
    /// Errors are not cached.
    fn get_or<E>(&self, key: Key, compute: impl FnOnce() -> Result<Value, E>) -> Result<Value, E> {
        if let Some(value) = self.inner.get(&key) {
            return Ok(value);
        }
        let value = compute()?;
        self.inner.insert(key, value.clone());
        Ok(value)
    }

    pub fn lines<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<Vec<String>, E>,
    ) -> Result<Arc<Vec<String>>, E> {
        match self.get_or(key, || compute().map(|v| Value::Lines(Arc::new(v))))? {
            Value::Lines(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn html<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<String, E>,
    ) -> Result<Arc<str>, E> {
        match self.get_or(key, || compute().map(|v| Value::Html(v.into())))? {
            Value::Html(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn tree<E>(
        &self,
        tree: ObjectId,
        compute: impl FnOnce() -> Result<Vec<TreeEntry>, E>,
    ) -> Result<Arc<Vec<TreeEntry>>, E> {
        match self.get_or(Key::Tree(tree), || {
            compute().map(|v| Value::Tree(Arc::new(v)))
        })? {
            Value::Tree(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn count<E>(&self, key: Key, compute: impl FnOnce() -> Result<u64, E>) -> Result<u64, E> {
        match self.get_or(key, || compute().map(Value::Count))? {
            Value::Count(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn refs<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<RefList, E>,
    ) -> Result<Arc<RefList>, E> {
        match self.get_or(key, || compute().map(|v| Value::Refs(Arc::new(v))))? {
            Value::Refs(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn log<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<LogPage, E>,
    ) -> Result<Arc<LogPage>, E> {
        match self.get_or(key, || compute().map(|v| Value::Log(Arc::new(v))))? {
            Value::Log(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn diff<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<CommitDiff, E>,
    ) -> Result<Arc<CommitDiff>, E> {
        match self.get_or(key, || compute().map(|v| Value::Diff(Arc::new(v))))? {
            Value::Diff(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn mailmap<E>(
        &self,
        blob: ObjectId,
        compute: impl FnOnce() -> Result<Mailmap, E>,
    ) -> Result<Arc<Mailmap>, E> {
        match self.get_or(Key::Mailmap(blob), || {
            compute().map(|v| Value::Mailmap(Arc::new(v)))
        })? {
            Value::Mailmap(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn blame<E>(
        &self,
        key: Key,
        compute: impl FnOnce() -> Result<Vec<BlameHunk>, E>,
    ) -> Result<Arc<Vec<BlameHunk>>, E> {
        match self.get_or(key, || compute().map(|v| Value::Blame(Arc::new(v))))? {
            Value::Blame(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn paths<E>(
        &self,
        tree: ObjectId,
        compute: impl FnOnce() -> Result<(Vec<String>, bool), E>,
    ) -> Result<Arc<(Vec<String>, bool)>, E> {
        match self.get_or(Key::Paths(tree), || {
            compute().map(|v| Value::Paths(Arc::new(v)))
        })? {
            Value::Paths(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    pub fn languages<E>(
        &self,
        tree: ObjectId,
        compute: impl FnOnce() -> Result<LanguageStats, E>,
    ) -> Result<Arc<LanguageStats>, E> {
        match self.get_or(Key::Languages(tree), || {
            compute().map(|v| Value::Languages(Arc::new(v)))
        })? {
            Value::Languages(v) => Ok(v),
            _ => unreachable!("cache key/value kind mismatch"),
        }
    }

    /// Language statistics if they are cached; never computes.
    pub fn peek_languages(&self, tree: ObjectId) -> Option<Arc<LanguageStats>> {
        match self.inner.get(&Key::Languages(tree))? {
            Value::Languages(v) => Some(v),
            _ => None,
        }
    }

    pub fn entry_count(&self) -> u64 {
        self.inner.run_pending_tasks();
        self.inner.entry_count()
    }
}
