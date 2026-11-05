//! Process-wide shared state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

use crate::cache::Cache;
use crate::config::{Config, ConfigError, Listen};
use crate::index::Indexer;
use crate::server::assets::Assets;

pub struct AppState {
    /// Current config; swapped atomically on reload.
    pub config: ArcSwap<Config>,
    /// Bumped on every successful reload; part of page ETags.
    pub config_generation: AtomicU64,
    pub assets: Assets,
    pub cache: Cache,
    /// Bounds concurrent git work (`limits.git_parallel`).
    pub git_permits: Semaphore,
    pub index: Indexer,
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
            git_permits: Semaphore::new(config.git_parallel()),
            config_generation: AtomicU64::new(0),
            config: ArcSwap::from_pointee(config),
            assets: Assets::new(),
            index: Indexer::new(),
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
        if new.server.compression != old.server.compression
            || new.limits.request_timeout_secs != old.limits.request_timeout_secs
        {
            tracing::warn!(
                "server.compression / limits.request_timeout_secs changed; restart required"
            );
            new.server.compression = old.server.compression;
            new.limits.request_timeout_secs = old.limits.request_timeout_secs;
        }
        if new.cache.size_mb != old.cache.size_mb || new.git_parallel() != old.git_parallel() {
            tracing::warn!("cache.size_mb / limits.git_parallel changed; restart required");
            new.cache.size_mb = old.cache.size_mb;
            new.limits.git_parallel = old.limits.git_parallel;
        }
        let new = Arc::new(new);
        self.config.store(new.clone());
        self.config_generation.fetch_add(1, Ordering::Relaxed);
        Ok(new)
    }

    /// Run blocking git work on the blocking pool, at most
    /// `limits.git_parallel` jobs at a time.
    pub async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&AppState) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let _permit = self.git_permits.acquire().await?;
        let state = self.clone();
        Ok(tokio::task::spawn_blocking(move || work(&state)).await?)
    }
}
