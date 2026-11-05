//! stderr logging. Under systemd (`JOURNAL_STREAM` set) timestamps and colors
//! are dropped because journald adds its own. `RUST_LOG` overrides
//! `log.level`; otherwise the level follows the config across reloads.

use std::io::IsTerminal;
use std::sync::OnceLock;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, reload};

use crate::config::Config;

static HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub fn init(config: &Config) {
    let (filter, handle) = reload::Layer::new(filter_for(config));
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    let registry = tracing_subscriber::registry().with(filter);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        registry.with(fmt.without_time().with_ansi(false)).init();
    } else {
        registry
            .with(fmt.with_ansi(std::io::stderr().is_terminal()))
            .init();
    }
    let _ = HANDLE.set(handle);
}

/// Apply `log.level` from a freshly loaded config.
pub fn apply(config: &Config) {
    if rust_log().is_some() {
        return;
    }
    if let Some(handle) = HANDLE.get() {
        let _ = handle.reload(filter_for(config));
    }
}

fn rust_log() -> Option<String> {
    std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn filter_for(config: &Config) -> EnvFilter {
    if let Some(spec) = rust_log() {
        match EnvFilter::try_new(&spec) {
            Ok(filter) => return filter,
            Err(e) => eprintln!("ignoring invalid RUST_LOG {spec:?}: {e}"),
        }
    }
    // Validated at config load time.
    EnvFilter::try_new(&config.log.level).unwrap_or_else(|_| EnvFilter::new("info"))
}
