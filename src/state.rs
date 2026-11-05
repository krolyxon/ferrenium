//! Process-wide shared state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::Cache;
use crate::config::{Config, ConfigError, Listen};
use crate::index::Indexer;
use crate::index::languages::Warmer;
use crate::server::assets::Assets;

pub struct AppState {
    /// Current config; swapped atomically on reload.
    pub config: ArcSwap<Config>,
    /// Bumped on every successful reload; part of page ETags.
    pub config_generation: AtomicU64,
    pub assets: Assets,
    pub cache: Cache,
    /// Bounds concurrent git work (`limits.git_parallel`).
    pub git_permits: Arc<Semaphore>,
    /// Bounds expensive requests: blame, archives, clones, log searches
    /// (`limits.heavy_parallel`). Held for a whole response stream.
    pub heavy_permits: Arc<Semaphore>,
    /// Bounds requests in flight (`limits.max_requests`).
    pub request_permits: Arc<Semaphore>,
    pub index: Indexer,
    /// Background language statistics.
    pub warmer: Warmer,
    /// Wakes the index watcher for an immediate full rescan.
    pub rescan: tokio::sync::Notify,
    /// Command-line overrides re-applied after every reload.
    pub overrides: Overrides,
}

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub config_path: Option<PathBuf>,
    pub listen: Option<Listen>,
}

impl Overrides {
    pub fn load(&self) -> Result<Config, ConfigError> {
        let mut config = Config::load(self.config_path.as_deref())?;
        if let Some(listen) = &self.listen {
            config.server.listen = listen.clone();
        }
        Ok(config)
    }
}

impl AppState {
    pub fn new(config: Config, overrides: Overrides) -> Arc<Self> {
        Arc::new(AppState {
            cache: Cache::new(config.cache.size_mb),
            git_permits: Arc::new(Semaphore::new(config.git_parallel())),
            heavy_permits: Arc::new(Semaphore::new(config.limits.heavy_parallel)),
            request_permits: Arc::new(Semaphore::new(config.limits.max_requests)),
            config_generation: AtomicU64::new(0),
            config: ArcSwap::from_pointee(config),
            assets: Assets::new(),
            index: Indexer::new(),
            warmer: Warmer::default(),
            rescan: tokio::sync::Notify::new(),
            overrides,
        })
    }

    /// Re-read the config file and swap it in. On error the running config
    /// stays in place. Returns the new config.
    pub fn reload(&self) -> Result<Arc<Config>, ConfigError> {
        let mut new = self.overrides.load()?;
        let old = self.config.load();
        // Settings baked into the listener and middleware stack keep their
        // running values so the config always describes the live server.
        if new.server.listen != old.server.listen {
            tracing::warn!(
                "server.listen changed to {}; restart required to take effect",
                new.server.listen
            );
            new.server.listen = old.server.listen.clone();
        }
        if new.server.base_path != old.server.base_path {
            tracing::warn!("server.base_path changed; restart required to take effect");
            new.server.base_path = old.server.base_path.clone();
        }
        if new.server.compression != old.server.compression {
            tracing::warn!("server.compression changed; restart required");
            new.server.compression = old.server.compression;
        }
        if new.cache.size_mb != old.cache.size_mb
            || new.git_parallel() != old.git_parallel()
            || new.limits.heavy_parallel != old.limits.heavy_parallel
            || new.limits.max_requests != old.limits.max_requests
        {
            tracing::warn!(
                "cache.size_mb / limits.git_parallel / limits.heavy_parallel / limits.max_requests \
                 changed; restart required"
            );
            new.limits.max_requests = old.limits.max_requests;
            new.cache.size_mb = old.cache.size_mb;
            new.limits.git_parallel = old.limits.git_parallel;
            new.limits.heavy_parallel = old.limits.heavy_parallel;
        }
        let new = Arc::new(new);
        self.config.store(new.clone());
        self.config_generation.fetch_add(1, Ordering::Relaxed);
        Ok(new)
    }

    /// A heavy-request permit, waiting at most `limits.heavy_wait_secs`.
    /// `None` when the server is too busy (or shutting down).
    pub async fn heavy_permit(&self) -> Option<OwnedSemaphorePermit> {
        let wait = Duration::from_secs(self.config.load().limits.heavy_wait_secs);
        match tokio::time::timeout(wait, self.heavy_permits.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Some(permit),
            _ => None,
        }
    }

    /// Run blocking git work on the blocking pool, at most
    /// `limits.git_parallel` jobs at a time.
    pub async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&AppState) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        // The permit moves into the job: a timed-out request stops waiting,
        // but the job keeps its slot until it really finishes.
        let permit = self.git_permits.clone().acquire_owned().await?;
        let state = self.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(&state)
        })
        .await?)
    }
}
