use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use ferrenium::config::{self, Config, Listen};
use ferrenium::index::Indexer;
use ferrenium::render::time;
use ferrenium::state::{AppState, Overrides};
use ferrenium::{examples, logging, server};

#[derive(Parser)]
#[command(
    name = "ferrenium",
    version,
    about = "A fast, read-only Git repository browser"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server in the foreground (this is what systemd runs).
    Serve {
        /// Config file [default: /etc/ferrenium/ferrenium.toml, then
        /// $XDG_CONFIG_HOME/ferrenium/ferrenium.toml]
        #[arg(long, env = "FERRENIUM_CONFIG")]
        config: Option<PathBuf>,
        /// Override server.listen (HOST:PORT or unix:/path/to.sock).
        #[arg(long)]
        listen: Option<Listen>,
    },
    /// Validate the config and report what would be served.
    Check {
        #[arg(long, env = "FERRENIUM_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Print a ready-to-use template to stdout.
    Example {
        #[arg(value_enum)]
        kind: ExampleKind,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ExampleKind {
    /// ferrenium.toml with every option.
    Config,
    /// Sandboxed systemd unit.
    Systemd,
    /// Caddy site block.
    Caddy,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { config, listen } => serve(config, listen),
        Command::Check { config } => check(config),
        Command::Example { kind } => {
            print!(
                "{}",
                match kind {
                    ExampleKind::Config => examples::CONFIG,
                    ExampleKind::Systemd => examples::SYSTEMD,
                    ExampleKind::Caddy => examples::CADDY,
                }
            );
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(config_path: Option<PathBuf>, listen: Option<Listen>) -> anyhow::Result<()> {
    let overrides = Overrides {
        config_path: config::resolve_path(config_path.as_deref()),
        listen,
    };
    let config = overrides.load()?;
    logging::init(&config);
    match &config.source {
        Some(path) => tracing::info!("config: {}", path.display()),
        None => tracing::warn!("no config file found; using built-in defaults"),
    }

    let state = AppState::new(config, overrides);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("ferrenium")
        .build()
        .context("cannot start async runtime")?
        .block_on(server::run(state))
}

fn check(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = config::resolve_path(config_path.as_deref());
    let config = Config::load(path.as_deref())?;
    let mut warnings = Vec::new();

    let source = match &config.source {
        Some(p) => p.display().to_string(),
        None => {
            warnings.push("no config file found; built-in defaults apply".to_string());
            "(built-in defaults)".to_string()
        }
    };
    println!("config     {source}");
    println!("listen     {}", config.server.listen);
    println!("base_path  {}", config.server.base_path);
    if let Some(url) = &config.server.base_url {
        println!("base_url   {url}");
    }
    println!(
        "scan_path  {} (max depth {})",
        config.repos.scan_path.display(),
        config.repos.max_depth
    );
    if !config.repos.scan_path.is_dir() {
        warnings.push(format!(
            "repos.scan_path {} is not a readable directory",
            config.repos.scan_path.display()
        ));
    }
    if config.server.base_url.is_none() {
        warnings.push("server.base_url is not set; feeds and clone URLs will be relative".into());
    }

    let indexer = Indexer::new();
    let stats = indexer.full_scan(&config);
    let index = indexer.load();
    println!(
        "\nrepositories  {} visible, {} hidden  (scanned in {:.1} ms)",
        stats.total,
        stats.hidden,
        stats.elapsed.as_secs_f64() * 1000.0
    );
    if !index.repos.is_empty() {
        let width = index
            .repos
            .keys()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        let now = time::now();
        for repo in index.repos.values() {
            let m = &repo.meta;
            println!(
                "  {:<width$}  {:<12}  {:>3} br {:>3} tags  {:<14}  {}",
                repo.name,
                m.default_branch.as_deref().unwrap_or("-"),
                m.branches,
                m.tags,
                m.last_activity
                    .map_or_else(|| "empty".into(), |t| time::relative(t, now)),
                m.section
                    .as_deref()
                    .map(|s| format!("[{s}]"))
                    .unwrap_or_default(),
            );
        }
    }
    for name in &index.hidden {
        println!("  {name}  (hidden)");
    }
    warnings.extend(index.warnings.iter().cloned());

    println!();
    for w in &warnings {
        println!("warning: {w}");
    }
    println!("ok");
    Ok(())
}
