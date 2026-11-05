//! HTTP server: router, middleware, listeners, signals and shutdown.

pub mod assets;
pub mod dispatch;
pub mod headers;

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tower::{Layer, ServiceBuilder};
use tower_http::CompressionLevel;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::predicate::{NotForContentType, Predicate};
use tower_http::compression::{CompressionLayer, DefaultPredicate};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Listen;
use crate::state::AppState;
use crate::{handlers, index};

/// How long in-flight requests may run after SIGTERM before we exit anyway.
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// Build the full application (routes + middleware) for the current config.
pub fn app(state: Arc<AppState>) -> Router {
    let config = state.config.load();

    let routes = Router::new()
        .route("/", get(handlers::index::index))
        .route("/-/health", get(health))
        .route("/-/site.css", get(assets::site_css))
        .route("/-/static/{hash}/{*path}", get(assets::serve))
        .route("/{*path}", get(dispatch::dispatch))
        .fallback(handlers::error::not_found)
        .with_state(state.clone());

    // The prefix must be stripped *before* routing, so wrap the whole router
    // (Router::layer would run after route matching).
    let routes = match config.server.base_path.as_str() {
        "/" => routes,
        base => Router::new().fallback_service(
            middleware::from_fn_with_state(
                Arc::<str>::from(base.trim_end_matches('/')),
                strip_base_path,
            )
            .layer(routes),
        ),
    };

    let trusted_proxy = config.server.trusted_proxy;
    let compression = CompressionLayer::new()
        .no_deflate()
        .no_zstd()
        .quality(CompressionLevel::Precise(4));
    // Always installed (so the body type is fixed); disabling turns off every
    // encoding instead.
    let compression = if config.server.compression {
        compression
    } else {
        compression.no_gzip().no_br()
    }
    // woff2 is already compressed.
    .compress_when(DefaultPredicate::new().and(NotForContentType::const_new("font/")));

    let [csp, nosniff, referrer, frame] = headers::layers();
    routes.layer(
        ServiceBuilder::new()
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(move |req: &Request| {
                        let client = trusted_proxy
                            .then(|| forwarded_for(req))
                            .flatten()
                            .unwrap_or("-");
                        tracing::info_span!(
                            "request",
                            method = %req.method(),
                            uri = %req.uri(),
                            client = %client,
                        )
                    })
                    .on_request(())
                    .on_response(DefaultOnResponse::new().level(Level::INFO)),
            )
            .layer(CatchPanicLayer::new())
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                Duration::from_secs(config.limits.request_timeout_secs),
            ))
            .layer(compression)
            .layer(csp)
            .layer(nosniff)
            .layer(referrer)
            .layer(frame),
    )
}

/// First address in `X-Forwarded-For` (the original client, as seen by the
/// trusted proxy).
fn forwarded_for(req: &Request) -> Option<&str> {
    let value = req.headers().get("x-forwarded-for")?.to_str().ok()?;
    value
        .split(',')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Serve under `server.base_path`: strip the prefix before routing, redirect
/// the bare prefix to its slash form, and 404 everything outside it.
async fn strip_base_path(State(base): State<Arc<str>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == &*base {
        return Redirect::permanent(&format!("{base}/")).into_response();
    }
    let Some(rest) = path.strip_prefix(&*base).filter(|r| r.starts_with('/')) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let new_uri = match req.uri().query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };
    match new_uri.parse::<Uri>() {
        Ok(uri) => {
            *req.uri_mut() = uri;
            next.run(req).await
        }
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn health() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        "ok\n",
    )
}

/// Run the server until SIGTERM/SIGINT. SIGHUP reloads the config.
pub async fn run(state: Arc<AppState>) -> anyhow::Result<()> {
    let listen = state.config.load().server.listen.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(handle_signals(state.clone(), shutdown_tx)?);

    // The index is complete before we report READY.
    let st = state.clone();
    let stats = tokio::task::spawn_blocking(move || {
        let config = st.config.load_full();
        st.index.full_scan(&config)
    })
    .await
    .context("initial scan failed")?;
    let mut seen = Default::default();
    index::watch::log_scan(&stats, &state.index.load(), &mut seen, true);
    tokio::spawn(index::watch::run(state.clone(), seen));
    // Parse the embedded grammars now rather than on the first blob view.
    tokio::task::spawn_blocking(|| {
        let start = std::time::Instant::now();
        crate::render::highlight::syntaxes();
        tracing::debug!("syntaxes loaded in {:?}", start.elapsed());
    });

    let app = app(state.clone());

    match &listen {
        Listen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot listen on {addr}"))?;
            serve(listener, app, &listen, shutdown_rx).await
        }
        Listen::Unix(path) => {
            let listener = bind_unix(path)?;
            let result = serve(listener, app, &listen, shutdown_rx).await;
            let _ = std::fs::remove_file(path);
            result
        }
    }
}

async fn serve<L>(
    listener: L,
    app: Router,
    listen: &Listen,
    mut shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    tracing::info!("listening on {listen}");
    notify(&[NotifyState::Ready, NotifyState::Status("serving")]);

    let mut drain_rx = shutdown_rx.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        })
        .into_future();

    tokio::select! {
        result = server => result.context("server error")?,
        _ = async {
            let _ = drain_rx.wait_for(|stop| *stop).await;
            tokio::time::sleep(DRAIN_DEADLINE).await;
        } => {
            tracing::warn!(
                "connections still open after {}s; exiting anyway",
                DRAIN_DEADLINE.as_secs()
            );
        }
    }
    tracing::info!("stopped");
    Ok(())
}

/// Bind a unix socket, replacing a stale socket file from a previous run.
fn bind_unix(path: &std::path::Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            meta.file_type().is_socket(),
            "{} exists and is not a socket",
            path.display()
        );
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("cannot listen on unix:{}", path.display()))?;
    // Same reachability as a loopback TCP port: any local user (i.e. the
    // reverse proxy) may connect. All content served is public anyway.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    Ok(listener)
}

/// Install signal handlers (synchronously, so a failure aborts startup) and
/// return the task that services them.
fn handle_signals(
    state: Arc<AppState>,
    shutdown_tx: watch::Sender<bool>,
) -> anyhow::Result<impl Future<Output = ()> + Send + 'static> {
    let mut term = signal(SignalKind::terminate()).context("cannot install SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("cannot install SIGINT handler")?;
    let mut hup = signal(SignalKind::hangup()).context("cannot install SIGHUP handler")?;

    Ok(async move {
        loop {
            tokio::select! {
                _ = term.recv() => { tracing::info!("SIGTERM received; draining"); break; }
                _ = int.recv() => { tracing::info!("SIGINT received; draining"); break; }
                _ = hup.recv() => reload(&state),
            }
        }
        notify(&[NotifyState::Stopping]);
        let _ = shutdown_tx.send(true);
    })
}

fn reload(state: &AppState) {
    tracing::info!("SIGHUP received; reloading configuration");
    let mut msg = vec![NotifyState::Reloading];
    if let Ok(now) = NotifyState::monotonic_usec_now() {
        msg.push(now);
    }
    notify(&msg);
    match state.reload() {
        Ok(config) => {
            crate::logging::apply(&config);
            tracing::info!("configuration reloaded");
            state.rescan.notify_one();
        }
        Err(e) => tracing::error!("reload failed, keeping current configuration: {e}"),
    }
    notify(&[NotifyState::Ready]);
}

fn notify(states: &[NotifyState]) {
    if let Err(e) = sd_notify::notify(states) {
        tracing::debug!("sd_notify failed: {e}");
    }
}
