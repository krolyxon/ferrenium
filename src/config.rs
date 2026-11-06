//! Configuration schema, defaults, validation and loading.
//!
//! Every key is optional. Unknown keys are rejected so typos surface at load
//! time instead of being silently ignored.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;

/// Well-known config location used in production.
pub const SYSTEM_CONFIG: &str = "/etc/ferrenium/ferrenium.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub site: SiteConfig,
    pub repos: ReposConfig,
    /// `[[repo]]` per-repository overrides.
    #[serde(rename = "repo")]
    pub repo_overrides: Vec<RepoOverride>,
    pub ui: UiConfig,
    pub cache: CacheConfig,
    pub watch: WatchConfig,
    pub limits: LimitsConfig,
    pub log: LogConfig,

    /// File the config was loaded from (`None` = built-in defaults).
    #[serde(skip)]
    pub source: Option<PathBuf>,
    /// Contents of `custom_css` / `header` / `footer` / `favicon` / `logo`,
    /// read at load time so that a reload picks up edits and requests never
    /// touch the filesystem.
    #[serde(skip)]
    pub custom: Customization,
}

#[derive(Debug, Clone, Default)]
pub struct Customization {
    pub css: Option<Arc<str>>,
    pub header: Option<Arc<str>>,
    pub footer: Option<Arc<str>>,
    pub favicon: Option<SiteImage>,
    pub logo: Option<SiteImage>,
}

/// An image from `site.favicon` / `site.logo`, served from memory.
#[derive(Debug, Clone)]
pub struct SiteImage {
    pub data: bytes::Bytes,
    pub content_type: &'static str,
    /// Short content hash for the `?v=` cache-busting query.
    pub version: String,
}

/// Largest accepted `site.favicon` / `site.logo` file.
const MAX_IMAGE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: Listen,
    pub base_url: Option<String>,
    pub base_path: String,
    pub trusted_proxy: bool,
    pub compression: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: Listen::Tcp(SocketAddr::from(([127, 0, 0, 1], 8080))),
            base_url: None,
            base_path: "/".into(),
            trusted_proxy: false,
            compression: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteConfig {
    pub title: String,
    pub description: String,
    pub clone_urls: Vec<String>,
    pub custom_css: Option<PathBuf>,
    pub header: Option<PathBuf>,
    pub footer: Option<PathBuf>,
    pub favicon: Option<PathBuf>,
    pub logo: Option<PathBuf>,
    /// Directory served at `/-/site/` for files used by `custom_css`.
    pub assets_dir: Option<PathBuf>,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            title: "ferrenium".into(),
            description: String::new(),
            clone_urls: Vec::new(),
            custom_css: None,
            header: None,
            footer: None,
            favicon: None,
            logo: None,
            assets_dir: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReposConfig {
    pub scan_path: PathBuf,
    pub max_depth: u32,
    pub follow_symlinks: bool,
    pub section_from_path: bool,
    pub hide: Vec<String>,
    pub snapshots: Vec<SnapshotFormat>,
    pub https_clone: bool,
}

impl Default for ReposConfig {
    fn default() -> Self {
        Self {
            scan_path: "/home/git".into(),
            max_depth: 3,
            follow_symlinks: false,
            section_from_path: false,
            hide: Vec::new(),
            snapshots: vec![SnapshotFormat::TarGz, SnapshotFormat::Zip],
            https_clone: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SnapshotFormat {
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "zip")]
    Zip,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoOverride {
    pub name: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub section: Option<String>,
    pub hidden: Option<bool>,
    pub readme: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiConfig {
    pub theme: Theme,
    /// Default order of the repository index (`?sort=` overrides it).
    pub index_sort: IndexSort,
    pub code_font: String,
    /// `#rgb` or `#rrggbb`; replaces the default red accent.
    pub accent: Option<String>,
    pub max_blob_kb: u64,
    pub max_highlight_kb: u64,
    /// Commits per log page.
    pub log_page: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Auto,
            index_sort: IndexSort::Activity,
            code_font: DEFAULT_CODE_FONT.into(),
            accent: None,
            max_blob_kb: 1024,
            max_highlight_kb: 256,
            log_page: 50,
        }
    }
}

pub const DEFAULT_CODE_FONT: &str = "Space Mono";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexSort {
    /// Most recently pushed first, as one list.
    #[default]
    Activity,
    /// By section, then name.
    Name,
}

impl IndexSort {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexSort::Activity => "activity",
            IndexSort::Name => "name",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    pub size_mb: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { size_mb: 64 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    /// Full rescan interval; 0 disables polling.
    pub poll_interval_secs: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 500,
            poll_interval_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// 0 = available CPUs, at most 8.
    pub git_parallel: usize,
    pub heavy_parallel: usize,
    /// Seconds a heavy request may wait for a permit before a 503.
    pub heavy_wait_secs: u64,
    /// Requests in flight at once; more are refused with a 503.
    pub max_requests: usize,
    pub request_timeout_secs: u64,
    /// Commits a filtered log (path or grep) examines per page.
    pub log_scan: usize,
    /// Above these, a commit's diff is not shown in full.
    pub diff_files: usize,
    pub diff_lines: usize,
    pub diff_file_lines: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            git_parallel: 0,
            heavy_parallel: 2,
            heavy_wait_secs: 10,
            max_requests: 512,
            request_timeout_secs: 30,
            log_scan: 10_000,
            diff_files: 300,
            diff_lines: 10_000,
            diff_file_lines: 2_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

/// Where the server listens: a TCP socket address or `unix:/abs/path`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl FromStr for Listen {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(format!("unix socket path must be absolute: {s:?}"));
            }
            return Ok(Listen::Unix(path));
        }
        s.parse::<SocketAddr>().map(Listen::Tcp).map_err(|_| {
            format!("invalid listen address {s:?} (expected HOST:PORT or unix:/path/to.sock)")
        })
    }
}

impl TryFrom<String> for Listen {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Listen::Tcp(addr) => write!(f, "http://{addr}"),
            Listen::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// Find the config file: explicit path (flag or `$FERRENIUM_CONFIG`), then the
/// system location, then `$XDG_CONFIG_HOME/ferrenium/ferrenium.toml`.
///
/// Returns `None` when no explicit path was given and no file exists, in which
/// case the built-in defaults apply.
pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let system = PathBuf::from(SYSTEM_CONFIG);
    if system.is_file() {
        return Some(system);
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let user = xdg.join("ferrenium/ferrenium.toml");
    user.is_file().then_some(user)
}

impl Config {
    /// Load, normalize and validate the config at `path` (or the defaults).
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                let mut config = Config::parse(&text).map_err(|message| ConfigError::Parse {
                    path: path.to_path_buf(),
                    message,
                })?;
                config.source = Some(path.to_path_buf());
                config
            }
            None => Config::default(),
        };
        config.finish()?;
        Ok(config)
    }

    /// Parse TOML without validation. Errors carry line/column and a snippet.
    pub fn parse(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Directory that relative paths in the config are resolved against.
    pub fn base_dir(&self) -> PathBuf {
        match self.source.as_deref().and_then(Path::parent) {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Normalize values, resolve relative paths, read customization files and
    /// validate. All problems are reported together.
    pub fn finish(&mut self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        let base_dir = self.base_dir();

        // server
        let bp = self.server.base_path.trim();
        if !bp.starts_with('/') {
            errors.push(format!("server.base_path must start with '/': {bp:?}"));
        } else if bp.contains("//") || bp.split('/').any(|s| s == "." || s == "..") {
            errors.push(format!("server.base_path is not a clean path: {bp:?}"));
        }
        let mut bp = bp.trim_end_matches('/').to_string();
        bp.push('/');
        self.server.base_path = bp;

        if let Some(url) = &mut self.server.base_url {
            let trimmed = url.trim().trim_end_matches('/').to_string();
            if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
                errors.push(format!(
                    "server.base_url must start with http:// or https://: {url:?}"
                ));
            }
            *url = trimmed;
        }

        // site
        if self.site.title.trim().is_empty() {
            errors.push("site.title must not be empty".into());
        }
        for (i, url) in self.site.clone_urls.iter().enumerate() {
            if !url.contains("{name}") {
                errors.push(format!(
                    "site.clone_urls[{i}] must contain the {{name}} placeholder: {url:?}"
                ));
            }
        }
        self.custom = Customization {
            css: read_custom(
                &base_dir,
                "site.custom_css",
                &mut self.site.custom_css,
                &mut errors,
            ),
            header: read_custom(&base_dir, "site.header", &mut self.site.header, &mut errors),
            footer: read_custom(&base_dir, "site.footer", &mut self.site.footer, &mut errors),
            favicon: read_image(
                &base_dir,
                "site.favicon",
                &mut self.site.favicon,
                &mut errors,
            ),
            logo: read_image(&base_dir, "site.logo", &mut self.site.logo, &mut errors),
        };

        if let Some(dir) = &mut self.site.assets_dir {
            // Canonical, so requests can check that symlinks stay inside it.
            match base_dir.join(&*dir).canonicalize() {
                Ok(canonical) if canonical.is_dir() => *dir = canonical,
                Ok(canonical) => errors.push(format!(
                    "site.assets_dir: {} is not a directory",
                    canonical.display()
                )),
                Err(e) => errors.push(format!(
                    "site.assets_dir: cannot open {}: {e}",
                    base_dir.join(&*dir).display()
                )),
            }
        }

        // repos
        if self.repos.scan_path.is_relative() {
            self.repos.scan_path = base_dir.join(&self.repos.scan_path);
        }
        if !(1..=16).contains(&self.repos.max_depth) {
            errors.push(format!(
                "repos.max_depth must be between 1 and 16, got {}",
                self.repos.max_depth
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, repo) in self.repo_overrides.iter_mut().enumerate() {
            let name = repo.name.trim().trim_matches('/').trim_end_matches(".git");
            if name.is_empty() {
                errors.push(format!("[[repo]] #{} has an empty name", i + 1));
                continue;
            }
            repo.name = name.to_string();
            if !seen.insert(repo.name.clone()) {
                errors.push(format!(
                    "[[repo]] {:?} is defined more than once",
                    repo.name
                ));
            }
        }

        // ui
        if self.ui.code_font.trim().is_empty() {
            errors.push("ui.code_font must not be empty".into());
        } else if self
            .ui
            .code_font
            .chars()
            .any(|c| matches!(c, '"' | '\'' | ';' | '{' | '}' | '<' | '>' | '\\'))
        {
            errors.push(format!(
                "ui.code_font contains characters not allowed in a font name: {:?}",
                self.ui.code_font
            ));
        }
        if let Some(accent) = &mut self.ui.accent {
            *accent = accent.trim().to_ascii_lowercase();
            if parse_hex_color(accent).is_none() {
                errors.push(format!(
                    "ui.accent must be a hex color like \"#d71921\": {accent:?}"
                ));
            }
        }
        if self.ui.max_blob_kb == 0 {
            errors.push("ui.max_blob_kb must be greater than 0".into());
        }

        // cache / watch / limits
        if self.cache.size_mb == 0 {
            errors.push("cache.size_mb must be greater than 0".into());
        }
        if self.watch.debounce_ms > 60_000 {
            errors.push("watch.debounce_ms must be at most 60000".into());
        }
        if self.limits.heavy_parallel == 0 {
            errors.push("limits.heavy_parallel must be at least 1".into());
        }
        if self.limits.max_requests == 0 {
            errors.push("limits.max_requests must be at least 1".into());
        }
        if self.limits.request_timeout_secs == 0 {
            errors.push("limits.request_timeout_secs must be at least 1".into());
        }
        if !(1..=1000).contains(&self.ui.log_page) {
            errors.push("ui.log_page must be between 1 and 1000".into());
        }
        for (key, value) in [
            ("log_scan", self.limits.log_scan),
            ("diff_files", self.limits.diff_files),
            ("diff_lines", self.limits.diff_lines),
            ("diff_file_lines", self.limits.diff_file_lines),
        ] {
            if value == 0 {
                errors.push(format!("limits.{key} must be at least 1"));
            }
        }

        // log
        if let Err(e) = tracing_subscriber::EnvFilter::try_new(&self.log.level) {
            errors.push(format!("log.level {:?} is invalid: {e}", self.log.level));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }

    /// Effective number of concurrent git workers.
    pub fn git_parallel(&self) -> usize {
        match self.limits.git_parallel {
            // Git work is CPU-bound; more workers than cores only add memory.
            0 => std::thread::available_parallelism().map_or(4, |n| n.get().min(8)),
            n => n,
        }
    }
}

/// Resolve a customization file path against `base_dir` and read it.
fn read_custom(
    base_dir: &Path,
    key: &str,
    path: &mut Option<PathBuf>,
    errors: &mut Vec<String>,
) -> Option<Arc<str>> {
    let p = path.as_mut()?;
    if p.is_relative() {
        *p = base_dir.join(&*p);
    }
    match std::fs::read_to_string(&*p) {
        Ok(text) => Some(text.into()),
        Err(e) => {
            errors.push(format!("{key}: cannot read {}: {e}", p.display()));
            None
        }
    }
}

/// Resolve an image path against `base_dir`, check its type and size, and
/// read it.
fn read_image(
    base_dir: &Path,
    key: &str,
    path: &mut Option<PathBuf>,
    errors: &mut Vec<String>,
) -> Option<SiteImage> {
    let p = path.as_mut()?;
    if p.is_relative() {
        *p = base_dir.join(&*p);
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let content_type = match ext.as_deref() {
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => {
            errors.push(format!(
                "{key}: unsupported image type (use .ico, .png, .svg, .webp, .gif or .jpg): {}",
                p.display()
            ));
            return None;
        }
    };
    let data = match std::fs::metadata(&*p) {
        Ok(m) if m.len() > MAX_IMAGE_BYTES => {
            errors.push(format!(
                "{key}: {} is larger than {} KiB",
                p.display(),
                MAX_IMAGE_BYTES / 1024
            ));
            return None;
        }
        Ok(_) => std::fs::read(&*p),
        Err(e) => Err(e),
    };
    match data {
        Ok(data) => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::hash::DefaultHasher::new();
            data.hash(&mut hasher);
            Some(SiteImage {
                data: data.into(),
                content_type,
                version: format!("{:08x}", hasher.finish() as u32),
            })
        }
        Err(e) => {
            errors.push(format!("{key}: cannot read {}: {e}", p.display()));
            None
        }
    }
}

/// `#rgb` / `#rrggbb` to its components.
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize, len: usize| u8::from_str_radix(&hex[i * len..(i + 1) * len], 16).ok();
    match hex.len() {
        3 => Some([digit(0, 1)? * 17, digit(1, 1)? * 17, digit(2, 1)? * 17]),
        6 => Some([digit(0, 2)?, digit(1, 2)?, digit(2, 2)?]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(text: &str) -> Result<Config, ConfigError> {
        let mut c = Config::parse(text).map_err(|message| ConfigError::Parse {
            path: "test.toml".into(),
            message,
        })?;
        c.finish()?;
        Ok(c)
    }

    #[test]
    fn empty_config_uses_defaults() {
        let c = finish("").unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(c.server.base_path, "/");
        assert_eq!(c.repos.scan_path, PathBuf::from("/home/git"));
        assert_eq!(c.repos.max_depth, 3);
        assert_eq!(c.ui.theme, Theme::Auto);
        assert_eq!(c.ui.index_sort, IndexSort::Activity);
        assert_eq!(c.cache.size_mb, 64);
        assert_eq!(
            c.repos.snapshots,
            vec![SnapshotFormat::TarGz, SnapshotFormat::Zip]
        );
        assert!(c.git_parallel() >= 1);
    }

    #[test]
    fn example_config_is_valid() {
        let c = finish(crate::examples::CONFIG).unwrap();
        assert_eq!(c.site.title, "ferrenium");
    }

    #[test]
    fn unknown_keys_are_rejected_with_position() {
        let err = finish("[server]\nlisten = \"127.0.0.1:1\"\nlistn = 1\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field `listn`"), "{msg}");
        assert!(msg.contains("line 3"), "{msg}");

        let err = finish("[sever]\n").unwrap_err().to_string();
        assert!(err.contains("unknown field `sever`"), "{err}");
    }

    #[test]
    fn listen_parsing() {
        assert_eq!(
            "unix:/run/ferrenium/ferrenium.sock".parse::<Listen>(),
            Ok(Listen::Unix("/run/ferrenium/ferrenium.sock".into()))
        );
        assert!("unix:relative.sock".parse::<Listen>().is_err());
        assert!("localhost".parse::<Listen>().is_err());
        assert_eq!(
            "[::1]:9000".parse::<Listen>(),
            Ok(Listen::Tcp("[::1]:9000".parse().unwrap()))
        );
        let err = finish("[server]\nlisten = \"nope\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid listen address"), "{err}");
    }

    #[test]
    fn base_path_and_url_are_normalized() {
        let c = finish("[server]\nbase_path = \"/git\"\nbase_url = \"https://x.org/\"\n").unwrap();
        assert_eq!(c.server.base_path, "/git/");
        assert_eq!(c.server.base_url.as_deref(), Some("https://x.org"));
        assert_eq!(
            finish("[server]\nbase_path = \"/\"\n")
                .unwrap()
                .server
                .base_path,
            "/"
        );
    }

    #[test]
    fn validation_reports_all_errors() {
        let err = finish(
            r#"
            [server]
            base_path = "git"
            base_url = "ftp://x"
            [repos]
            max_depth = 0
            [ui]
            theme = "dark"
            code_font = "x; } body { display: none"
            [cache]
            size_mb = 0
            [[repo]]
            name = "a.git"
            [[repo]]
            name = "a"
            "#,
        )
        .unwrap_err();
        let ConfigError::Invalid(errors) = err else {
            panic!("expected validation errors, got {err}");
        };
        let all = errors.join("\n");
        for needle in [
            "base_path",
            "base_url",
            "max_depth",
            "code_font",
            "size_mb",
            "defined more than once",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
    }

    #[test]
    fn invalid_enum_values_are_rejected() {
        assert!(finish("[ui]\ntheme = \"purple\"\n").is_err());
        assert!(finish("[repos]\nsnapshots = [\"rar\"]\n").is_err());
    }

    #[test]
    fn custom_files_resolve_relative_to_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("custom.css"), "body{}").unwrap();
        let path = dir.path().join("ferrenium.toml");
        std::fs::write(
            &path,
            "[site]\ncustom_css = \"custom.css\"\n[repos]\nscan_path = \"repos\"\n",
        )
        .unwrap();
        let c = Config::load(Some(&path)).unwrap();
        assert_eq!(c.custom.css.as_deref(), Some("body{}"));
        assert_eq!(c.repos.scan_path, dir.path().join("repos"));

        std::fs::write(&path, "[site]\nfooter = \"missing.html\"\n").unwrap();
        let err = Config::load(Some(&path)).unwrap_err().to_string();
        assert!(err.contains("site.footer"), "{err}");
    }

    #[test]
    fn images_are_read_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("icon.PNG"), b"\x89PNG").unwrap();
        std::fs::write(dir.path().join("logo.bmp"), b"BM").unwrap();
        std::fs::write(
            dir.path().join("big.svg"),
            vec![b' '; MAX_IMAGE_BYTES as usize + 1],
        )
        .unwrap();
        let path = dir.path().join("ferrenium.toml");
        std::fs::write(&path, "[site]\nfavicon = \"icon.PNG\"\n").unwrap();
        let c = Config::load(Some(&path)).unwrap();
        let icon = c.custom.favicon.unwrap();
        assert_eq!(icon.content_type, "image/png");
        assert_eq!(&icon.data[..], b"\x89PNG");
        assert_eq!(icon.version.len(), 8);

        std::fs::write(
            &path,
            "[site]\nfavicon = \"big.svg\"\nlogo = \"logo.bmp\"\n",
        )
        .unwrap();
        let err = Config::load(Some(&path)).unwrap_err().to_string();
        assert!(
            err.contains("site.favicon") && err.contains("larger"),
            "{err}"
        );
        assert!(
            err.contains("site.logo") && err.contains("unsupported"),
            "{err}"
        );
    }

    #[test]
    fn assets_dir_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let path = dir.path().join("ferrenium.toml");
        std::fs::write(&path, "[site]\nassets_dir = \"assets\"\n").unwrap();
        let c = Config::load(Some(&path)).unwrap();
        assert_eq!(
            c.site.assets_dir,
            Some(dir.path().join("assets").canonicalize().unwrap())
        );
        for bad in ["file", "missing"] {
            std::fs::write(&path, format!("[site]\nassets_dir = {bad:?}\n")).unwrap();
            let err = Config::load(Some(&path)).unwrap_err().to_string();
            assert!(err.contains("site.assets_dir"), "{err}");
        }
    }

    #[test]
    fn accent_colors() {
        assert_eq!(parse_hex_color("#d71921"), Some([215, 25, 33]));
        assert_eq!(parse_hex_color("#fA0"), Some([255, 170, 0]));
        for bad in ["d71921", "#d7192", "#gggggg", "red", "#+1+2+3"] {
            assert_eq!(parse_hex_color(bad), None, "{bad}");
        }
        let c = finish("[ui]\naccent = \" #FF5500 \"\n").unwrap();
        assert_eq!(c.ui.accent.as_deref(), Some("#ff5500"));
        let err = finish("[ui]\naccent = \"red; } body {\"\n").unwrap_err();
        assert!(err.to_string().contains("ui.accent"), "{err}");
    }
}
