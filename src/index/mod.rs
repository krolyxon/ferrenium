//! Repository discovery and the in-memory repository index.
//!
//! Readers get a consistent snapshot via [`Indexer::load`] without locking.
//! Writers (full scans and single-repo refreshes) are serialized and swap in
//! a new snapshot copy-on-write.

pub mod languages;
pub mod languages_table;
pub mod meta;
pub mod scan;
pub mod watch;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::git::ThreadSafeRepository;
use meta::{Inspected, RepoMeta};
use scan::Candidate;

pub struct Repo {
    pub name: String,
    pub git_dir: PathBuf,
    pub meta: RepoMeta,
    pub gix: ThreadSafeRepository,
    /// Changes whenever this repo's refs or metadata change; part of cache
    /// keys for ref-dependent data.
    pub generation: u64,
}

#[derive(Default, Clone)]
pub struct RepoIndex {
    /// Visible repositories by name.
    pub repos: BTreeMap<String, Arc<Repo>>,
    /// Every discovered git dir (hidden ones included) → name.
    pub git_dirs: BTreeMap<PathBuf, String>,
    pub hidden: BTreeSet<String>,
    /// Non-repository directories walked during discovery.
    pub dirs: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub total: usize,
    pub hidden: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub elapsed: Duration,
}

impl ScanStats {
    pub fn any_change(&self) -> bool {
        self.added + self.removed + self.changed > 0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refresh {
    Unchanged,
    Updated(String),
    Hidden(String),
    /// The path is not (or no longer) a known repository: do a full scan.
    NeedsRescan,
    Failed(String),
}

pub struct Indexer {
    current: ArcSwap<RepoIndex>,
    generation: AtomicU64,
    write: Mutex<()>,
}

impl Default for Indexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Indexer {
    pub fn new() -> Self {
        Indexer {
            current: ArcSwap::from_pointee(RepoIndex::default()),
            generation: AtomicU64::new(0),
            write: Mutex::new(()),
        }
    }

    pub fn load(&self) -> Arc<RepoIndex> {
        self.current.load_full()
    }

    /// Rediscover everything under the scan path and rebuild the index.
    pub fn full_scan(&self, config: &Config) -> ScanStats {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let old = self.current.load_full();
        let discovery = scan::discover(
            &config.repos.scan_path,
            config.repos.max_depth,
            config.repos.follow_symlinks,
        );

        let mut new = RepoIndex {
            dirs: discovery.dirs,
            warnings: discovery.warnings,
            ..RepoIndex::default()
        };
        let inspected = inspect_all(config, &discovery.candidates);
        for (candidate, result) in discovery.candidates.into_iter().zip(inspected) {
            new.git_dirs
                .insert(candidate.git_dir.clone(), candidate.name.clone());
            match result {
                Err(e) => new.warnings.push(format!("{}: {e:#}", candidate.name)),
                Ok(inspected) => {
                    new.warnings.extend(inspected.warnings.iter().cloned());
                    if inspected.hidden {
                        new.hidden.insert(candidate.name);
                    } else {
                        let repo = self.make_repo(&old, candidate, inspected);
                        new.repos.insert(repo.name.clone(), repo);
                    }
                }
            }
        }
        let known: BTreeSet<&String> = new.git_dirs.values().collect();
        for o in &config.repo_overrides {
            if !known.contains(&o.name) {
                new.warnings.push(format!(
                    "[[repo]] {:?} does not match any repository",
                    o.name
                ));
            }
        }

        let mut stats = ScanStats {
            total: new.repos.len(),
            hidden: new.hidden.len(),
            ..ScanStats::default()
        };
        for (name, repo) in &new.repos {
            match old.repos.get(name) {
                None => stats.added += 1,
                Some(prev) if !Arc::ptr_eq(prev, repo) => stats.changed += 1,
                Some(_) => {}
            }
        }
        stats.removed = old
            .repos
            .keys()
            .filter(|name| !new.repos.contains_key(*name))
            .count();
        self.current.store(Arc::new(new));
        stats.elapsed = start.elapsed();
        stats
    }

    /// Re-inspect the single repository at `git_dir` (after a ref change).
    pub fn refresh(&self, config: &Config, git_dir: &Path) -> Refresh {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let old = self.current.load_full();
        let Some(name) = old.git_dirs.get(git_dir).cloned() else {
            return Refresh::NeedsRescan;
        };
        if scan::repo_git_dir(git_dir).as_deref() != Some(git_dir) {
            return Refresh::NeedsRescan;
        }
        let candidate = Candidate {
            name: name.clone(),
            git_dir: git_dir.to_path_buf(),
        };
        let inspected = match meta::inspect(config, &candidate) {
            Ok(inspected) => inspected,
            // Keep serving the previous state; the next event or poll retries.
            Err(e) => return Refresh::Failed(format!("{name}: {e:#}")),
        };

        let mut new = (*old).clone();
        if inspected.hidden {
            if new.repos.remove(&name).is_none() {
                return Refresh::Unchanged;
            }
            new.hidden.insert(name.clone());
            self.current.store(Arc::new(new));
            return Refresh::Hidden(name);
        }
        let repo = self.make_repo(&old, candidate, inspected);
        if old
            .repos
            .get(&name)
            .is_some_and(|prev| Arc::ptr_eq(prev, &repo))
        {
            return Refresh::Unchanged;
        }
        new.hidden.remove(&name);
        new.repos.insert(name.clone(), repo);
        self.current.store(Arc::new(new));
        Refresh::Updated(name)
    }

    /// Reuse the previous entry when nothing changed, so its generation (and
    /// everything cached under it) stays valid.
    fn make_repo(&self, old: &RepoIndex, candidate: Candidate, inspected: Inspected) -> Arc<Repo> {
        if let Some(prev) = old.repos.get(&candidate.name)
            && prev.git_dir == candidate.git_dir
            && prev.meta == inspected.meta
        {
            return prev.clone();
        }
        Arc::new(Repo {
            name: candidate.name,
            git_dir: candidate.git_dir,
            meta: inspected.meta,
            gix: inspected.gix,
            generation: self.generation.fetch_add(1, Ordering::Relaxed) + 1,
        })
    }
}

/// Inspect candidates on a few threads; results keep the input order.
fn inspect_all(config: &Config, candidates: &[Candidate]) -> Vec<anyhow::Result<Inspected>> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(candidates.len())
        .max(1);
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, anyhow::Result<Inspected>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(candidate) = candidates.get(i) else {
                            break out;
                        };
                        out.push((i, meta::inspect(config, candidate)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("inspect worker panicked"))
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}
