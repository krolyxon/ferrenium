//! Keeps the index fresh: inotify watches (debounced per repository), a
//! periodic full rescan as a fallback, and rescans requested on SIGHUP.
//!
//! Watched paths:
//! - each git dir, non-recursively (`HEAD`, `packed-refs`, `config`,
//!   `description`) and its `refs/` recursively; objects are never watched;
//! - every non-repository directory walked during discovery, so repositories
//!   appearing or disappearing trigger a rescan.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use super::{Refresh, RepoIndex, ScanStats};
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Task {
    Rescan,
    Repo(PathBuf),
}

/// Run forever. Call after the initial scan; `seen` holds the warnings that
/// scan already logged.
pub async fn run(state: Arc<AppState>, mut seen: BTreeSet<String>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let mut fs = FsWatch::new(&state, tx);
    fs.sync(&state.index.load());

    let mut pending: HashMap<Task, Instant> = HashMap::new();
    let mut poll_secs = state.config.load().watch.poll_interval_secs;
    let mut poll = poll_timer(poll_secs);

    loop {
        let config = state.config.load_full();
        if config.watch.poll_interval_secs != poll_secs {
            poll_secs = config.watch.poll_interval_secs;
            poll = poll_timer(poll_secs);
        }
        let debounce = Duration::from_millis(config.watch.debounce_ms);
        let next_due = pending.values().min().copied();

        tokio::select! {
            Some(event) = rx.recv() => {
                let index = state.index.load();
                for task in classify(event, &index) {
                    pending.entry(task).or_insert_with(|| Instant::now() + debounce);
                }
            }
            _ = state.rescan.notified() => {
                pending.insert(Task::Rescan, Instant::now());
            }
            _ = async { poll.as_mut().unwrap().tick().await }, if poll.is_some() => {
                pending.entry(Task::Rescan).or_insert_with(Instant::now);
            }
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => {
                let now = Instant::now();
                let due: Vec<Task> = pending
                    .iter()
                    .filter(|(_, at)| **at <= now)
                    .map(|(task, _)| task.clone())
                    .collect();
                for task in &due {
                    pending.remove(task);
                }
                if due.contains(&Task::Rescan) {
                    // A full scan covers every per-repo refresh.
                    pending.retain(|task, _| *task == Task::Rescan);
                    let st = state.clone();
                    let stats = tokio::task::spawn_blocking(move || {
                        let config = st.config.load_full();
                        st.index.full_scan(&config)
                    })
                    .await;
                    if let Ok(stats) = stats {
                        let index = state.index.load();
                        log_scan(&stats, &index, &mut seen, false);
                        fs.sync(&index);
                    }
                    continue;
                }
                for task in due {
                    let Task::Repo(git_dir) = task else { continue };
                    let st = state.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        let config = st.config.load_full();
                        st.index.refresh(&config, &git_dir)
                    })
                    .await;
                    match outcome {
                        Ok(Refresh::Updated(name)) => tracing::info!("updated {name}"),
                        Ok(Refresh::Hidden(name)) => tracing::info!("{name} is now hidden"),
                        Ok(Refresh::NeedsRescan) => {
                            pending.insert(Task::Rescan, Instant::now());
                        }
                        Ok(Refresh::Failed(e)) => tracing::warn!("refresh failed: {e}"),
                        Ok(Refresh::Unchanged) | Err(_) => {}
                    }
                }
            }
        }
    }
}

fn poll_timer(secs: u64) -> Option<Interval> {
    (secs > 0).then(|| {
        let period = Duration::from_secs(secs);
        let mut interval = tokio::time::interval_at(Instant::now() + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval
    })
}

/// Map a filesystem event to the work it implies.
fn classify(event: notify::Result<Event>, index: &RepoIndex) -> Vec<Task> {
    let event = match event {
        Ok(event) => event,
        Err(e) => {
            tracing::warn!("watch error: {e}; rescanning");
            return vec![Task::Rescan];
        }
    };
    if event.need_rescan() {
        return vec![Task::Rescan];
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return Vec::new();
    }
    let mut tasks = Vec::new();
    for path in &event.paths {
        if path.extension().is_some_and(|ext| ext == "lock") {
            continue;
        }
        let task = if let Some(git_dir) = path.ancestors().find(|a| index.git_dirs.contains_key(*a))
        {
            Task::Repo(git_dir.to_path_buf())
        } else if index
            .dirs
            .iter()
            .any(|d| d == path || Some(d.as_path()) == path.parent())
        {
            Task::Rescan
        } else {
            continue;
        };
        if !tasks.contains(&task) {
            tasks.push(task);
        }
    }
    tasks
}

/// Log a scan: a summary when something changed (always for the initial
/// scan) and each warning once until it goes away.
pub fn log_scan(stats: &ScanStats, index: &RepoIndex, seen: &mut BTreeSet<String>, initial: bool) {
    let ms = stats.elapsed.as_secs_f64() * 1000.0;
    if initial {
        tracing::info!(
            "indexed {} repositories ({} hidden) in {ms:.1} ms",
            stats.total,
            stats.hidden
        );
    } else if stats.any_change() {
        tracing::info!(
            "rescan: {} repositories (+{} −{} ~{}) in {ms:.1} ms",
            stats.total,
            stats.added,
            stats.removed,
            stats.changed
        );
    } else {
        tracing::debug!("rescan: no changes ({ms:.1} ms)");
    }
    let current: BTreeSet<String> = index.warnings.iter().cloned().collect();
    for warning in current.difference(seen) {
        tracing::warn!("{warning}");
    }
    *seen = current;
}

/// The inotify watcher and the set of paths it currently watches.
struct FsWatch {
    watcher: Option<RecommendedWatcher>,
    watched: HashMap<PathBuf, RecursiveMode>,
    warned_limit: bool,
}

impl FsWatch {
    fn new(state: &AppState, tx: mpsc::UnboundedSender<notify::Result<Event>>) -> Self {
        let watcher = if state.config.load().watch.enabled {
            match notify::recommended_watcher(move |event| {
                let _ = tx.send(event);
            }) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!("cannot start file watcher ({e}); relying on periodic rescans");
                    None
                }
            }
        } else {
            None
        };
        FsWatch {
            watcher,
            watched: HashMap::new(),
            warned_limit: false,
        }
    }

    /// Bring the watched set in line with the index.
    fn sync(&mut self, index: &RepoIndex) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let mut desired: HashMap<PathBuf, RecursiveMode> = HashMap::new();
        for dir in &index.dirs {
            desired.insert(dir.clone(), RecursiveMode::NonRecursive);
        }
        for git_dir in index.git_dirs.keys() {
            desired.insert(git_dir.clone(), RecursiveMode::NonRecursive);
            desired.insert(git_dir.join("refs"), RecursiveMode::Recursive);
        }

        let stale: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(path, mode)| desired.get(*path) != Some(*mode))
            .map(|(path, _)| path.clone())
            .collect();
        for path in stale {
            let _ = watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        for (path, mode) in desired {
            if self.watched.contains_key(&path) {
                continue;
            }
            match watcher.watch(&path, mode) {
                Ok(()) => {
                    self.watched.insert(path, mode);
                }
                Err(e) => watch_failed(&mut self.warned_limit, &path, e),
            }
        }
    }
}

fn watch_failed(warned_limit: &mut bool, path: &Path, e: notify::Error) {
    // ENOSPC from inotify_add_watch means the per-user watch limit.
    let limit = matches!(&e.kind, notify::ErrorKind::MaxFilesWatch)
        || matches!(&e.kind, notify::ErrorKind::Io(io) if io.raw_os_error() == Some(28));
    if !limit {
        tracing::debug!("cannot watch {}: {e}", path.display());
    } else if !*warned_limit {
        *warned_limit = true;
        tracing::warn!(
            "inotify watch limit reached; raise fs.inotify.max_user_watches \
             (changes are still picked up by periodic rescans)"
        );
    }
}
