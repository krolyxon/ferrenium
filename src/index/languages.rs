//! Language statistics, the way GitHub counts them: bytes per language
//! over the files of a tree, counting only programming and markup
//! languages, and leaving out vendored, generated and documentation paths.
//! `linguist-*` attributes in the root `.gitattributes` override all of
//! that.
//!
//! Results are cached by tree id and computed in the background by
//! [`warm`], so pages never wait for them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use fancy_regex::Regex;
use gix::bstr::BStr;
use gix::glob::wildmatch;

use super::languages_table::{DOCUMENTATION, EXTENSIONS, FILENAMES, LANGUAGES, VENDOR};
use crate::cache::Cache;
use crate::git::tree::{self, EntryKind, PathTarget};
use crate::git::{self, ObjectId};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Programming,
    Markup,
    Data,
    Prose,
}

pub struct Language {
    pub name: &'static str,
    pub kind: Kind,
    /// Parent language, for statistics.
    pub group: Option<u16>,
}

/// Bytes per language, largest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub languages: Vec<(&'static str, u64)>,
    pub total: u64,
}

impl Stats {
    pub fn weight(&self) -> usize {
        64 + self.languages.len() * 32
    }

    pub fn primary(&self) -> Option<&'static str> {
        self.languages.first().map(|(name, _)| *name)
    }
}

/// Language of a path from its file name, then its extensions (longest
/// first: `x.d.ts` tries `.d.ts`, then `.ts`).
pub fn detect(path: &str) -> Option<&'static Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Ok(i) = FILENAMES.binary_search_by(|(f, _)| (*f).cmp(name)) {
        return Some(&LANGUAGES[FILENAMES[i].1 as usize]);
    }
    let lower = name.to_ascii_lowercase();
    // A leading dot is a hidden file, not an extension (`.bashrc`).
    for (i, _) in lower.match_indices('.').filter(|(i, _)| *i > 0) {
        let ext = &lower[i..];
        if let Ok(j) = EXTENSIONS.binary_search_by(|(e, _)| (*e).cmp(ext)) {
            return Some(&LANGUAGES[EXTENSIONS[j].1 as usize]);
        }
    }
    None
}

/// Paths of generated files (beyond Linguist's vendor/documentation lists).
const GENERATED: &[&str] = &[
    r"\.min\.(js|css|mjs)$",
    r"-min\.js$",
    r"\.(js|css)\.map$",
    r"\.pb\.(go|cc|h)$",
    r"_pb2(_grpc)?\.py$",
    r"\.designer\.(cs|vb)$",
    r"(^|/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|composer\.lock|Gemfile\.lock|poetry\.lock|go\.sum)$",
];

struct Rules {
    vendor: Regex,
    documentation: Regex,
    generated: Regex,
}

fn alternation(patterns: &[&str]) -> Regex {
    let joined = patterns
        .iter()
        .map(|p| format!("(?:{p})"))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&joined).expect("Linguist patterns compile")
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules {
        vendor: alternation(VENDOR),
        documentation: alternation(DOCUMENTATION),
        generated: alternation(GENERATED),
    })
}

fn matches(re: &Regex, path: &str) -> bool {
    re.is_match(path).unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Set,
    Unset,
    Is(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Set => true,
            Value::Unset => false,
            Value::Is(v) => !matches!(v.as_str(), "false" | "0" | "no"),
        }
    }
}

/// `linguist-*` rules from a `.gitattributes`.
#[derive(Debug, Default)]
pub struct Attributes {
    rules: Vec<(String, Vec<(String, Value)>)>,
}

impl Attributes {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern) = parts.next() else {
                continue;
            };
            let attrs: Vec<(String, Value)> = parts
                .filter_map(|a| {
                    let (name, value) = if let Some(name) = a.strip_prefix('-') {
                        (name, Value::Unset)
                    } else if let Some(name) = a.strip_prefix('!') {
                        (name, Value::Unset)
                    } else if let Some((name, v)) = a.split_once('=') {
                        (name, Value::Is(v.to_string()))
                    } else {
                        (a, Value::Set)
                    };
                    name.starts_with("linguist-")
                        .then(|| (name.to_string(), value))
                })
                .collect();
            if !attrs.is_empty() {
                rules.push((pattern.to_string(), attrs));
            }
        }
        Attributes { rules }
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The last matching value of `attr` for `path`.
    fn get(&self, path: &str, attr: &str) -> Option<&Value> {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.rules
            .iter()
            .rev()
            .filter(|(pattern, _)| {
                let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
                if pattern.contains('/') {
                    wildmatch(
                        BStr::new(pattern),
                        BStr::new(path),
                        wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
                    )
                } else {
                    wildmatch(
                        BStr::new(pattern),
                        BStr::new(name),
                        wildmatch::Mode::empty(),
                    )
                }
            })
            .find_map(|(_, attrs)| {
                attrs
                    .iter()
                    .rev()
                    .find(|(name, _)| name == attr)
                    .map(|(_, v)| v)
            })
    }

    fn flag(&self, path: &str, attr: &str) -> Option<bool> {
        self.get(path, attr).map(Value::truthy)
    }
}

/// Whether a file counts, and as which language.
fn classify(path: &str, attrs: &Attributes) -> Option<&'static str> {
    let rules = rules();
    let excluded =
        |attr: &str, re: &Regex| attrs.flag(path, attr).unwrap_or_else(|| matches(re, path));
    if excluded("linguist-vendored", &rules.vendor)
        || excluded("linguist-generated", &rules.generated)
        || excluded("linguist-documentation", &rules.documentation)
    {
        return None;
    }
    let forced = match attrs.get(path, "linguist-language") {
        Some(Value::Is(name)) => LANGUAGES.iter().find(|l| {
            l.name.eq_ignore_ascii_case(&name.replace('-', " "))
                || l.name.eq_ignore_ascii_case(name)
        }),
        _ => None,
    };
    let language = forced.or_else(|| detect(path))?;
    let detectable = attrs
        .flag(path, "linguist-detectable")
        .unwrap_or(matches!(language.kind, Kind::Programming | Kind::Markup));
    if !detectable {
        return None;
    }
    let counted = language.group.map_or(language, |g| &LANGUAGES[g as usize]);
    Some(counted.name)
}

/// Language statistics of the tree `tree` (with the root `.gitattributes`
/// of that tree).
pub fn compute(repo: &git::Handle, tree_id: ObjectId) -> anyhow::Result<Stats> {
    let attrs = match tree::list(repo, tree_id)?
        .iter()
        .find(|e| e.name == ".gitattributes" && e.kind.is_blob())
    {
        Some(entry) => Attributes::parse(&String::from_utf8_lossy(&tree::blob(repo, entry.oid)?)),
        None => Attributes::default(),
    };
    let rules = rules();
    // Directories can be skipped wholesale unless attributes might bring
    // something inside back.
    let prune = attrs.is_empty();
    let mut bytes: HashMap<&'static str, u64> = HashMap::new();
    tree::walk(
        repo,
        tree_id,
        &mut |dir| !(prune && (matches(&rules.vendor, dir) || matches(&rules.documentation, dir))),
        &mut |path, kind, size| {
            if kind == EntryKind::Symlink {
                return;
            }
            if let Some(name) = classify(path, &attrs) {
                *bytes.entry(name).or_default() += size;
            }
        },
    )?;
    let mut languages: Vec<(&'static str, u64)> =
        bytes.into_iter().filter(|(_, n)| *n > 0).collect();
    languages.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let total = languages.iter().map(|(_, n)| n).sum();
    Ok(Stats { languages, total })
}

/// Background state: wake-ups and each repository's main language (for
/// the index page).
#[derive(Default)]
pub struct Warmer {
    pub wake: tokio::sync::Notify,
    primary: Mutex<HashMap<String, &'static str>>,
}

impl Warmer {
    pub fn primary(&self, repo: &str) -> Option<&'static str> {
        self.primary
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(repo)
            .copied()
    }

    fn set_primary(&self, repo: &str, language: Option<&'static str>) {
        let mut map = self.primary.lock().unwrap_or_else(|e| e.into_inner());
        match language {
            Some(l) => {
                map.insert(repo.to_string(), l);
            }
            None => {
                map.remove(repo);
            }
        }
    }
}

/// Root tree of a commit.
pub fn root_tree(repo: &git::Handle, commit: ObjectId) -> anyhow::Result<ObjectId> {
    match tree::lookup(repo, commit, b"")? {
        Some(PathTarget::Tree(id)) => Ok(id),
        _ => anyhow::bail!("commit has no tree"),
    }
}

/// Cached statistics of `tree`, without computing them.
pub fn cached(cache: &Cache, tree: ObjectId) -> Option<Arc<Stats>> {
    cache.peek_languages(tree)
}

/// How often the warmer looks for repositories without statistics, besides
/// being woken by page views that found none.
const WARM_INTERVAL: Duration = Duration::from_secs(60);

/// Compute missing statistics for every repository's default branch, one
/// repository at a time, forever.
pub async fn warm(state: Arc<AppState>) {
    loop {
        let index = state.index.load();
        for repo in index.repos.values() {
            let Some(head) = repo.meta.head else {
                state.warmer.set_primary(&repo.name, None);
                continue;
            };
            let repo = repo.clone();
            let result = state
                .blocking(move |state| -> anyhow::Result<()> {
                    let git = git::handle(&repo.gix);
                    let tree = root_tree(&git, head)?;
                    let stats = match state.cache.peek_languages(tree) {
                        Some(stats) => stats,
                        None => {
                            let start = std::time::Instant::now();
                            let stats = state.cache.languages(tree, || compute(&git, tree))?;
                            tracing::debug!(
                                "{}: language stats in {:?}",
                                repo.name,
                                start.elapsed()
                            );
                            stats
                        }
                    };
                    state.warmer.set_primary(&repo.name, stats.primary());
                    Ok(())
                })
                .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) | Err(e) => tracing::warn!("language stats: {e:#}"),
            }
        }
        tokio::select! {
            _ = state.warmer.wake.notified() => {}
            _ = tokio::time::sleep(WARM_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        let name = |p: &str| detect(p).map(|l| l.name);
        assert_eq!(name("src/main.rs"), Some("Rust"));
        assert_eq!(name("Makefile"), Some("Makefile"));
        assert_eq!(name("a/b/Dockerfile"), Some("Dockerfile"));
        assert_eq!(name("x.PY"), Some("Python"));
        assert_eq!(name("types.d.ts"), Some("TypeScript"));
        assert_eq!(name("lib.h"), Some("C"));
        assert_eq!(name(".bashrc"), Some("Shell"));
        assert_eq!(name("LICENSE"), Some("Text"));
        assert_eq!(name("no-extension"), None);
    }

    #[test]
    fn exclusions_and_attributes() {
        let none = Attributes::default();
        assert_eq!(classify("src/main.rs", &none), Some("Rust"));
        assert_eq!(classify("vendor/lib.rs", &none), None);
        assert_eq!(classify("node_modules/x/index.js", &none), None);
        assert_eq!(classify("docs/conf.py", &none), None);
        assert_eq!(classify("app.min.js", &none), None);
        assert_eq!(classify("config.json", &none), None, "data");
        assert_eq!(classify("README.md", &none), None, "prose");

        let attrs = Attributes::parse(
            "# comment\n*.json linguist-detectable\nvendor/** -linguist-vendored\n\
             gen/*.rs linguist-generated=true\n*.inc linguist-language=C++\n",
        );
        assert_eq!(classify("config.json", &attrs), Some("JSON"));
        assert_eq!(classify("vendor/lib.rs", &attrs), Some("Rust"));
        assert_eq!(classify("gen/x.rs", &attrs), None);
        assert_eq!(classify("a/b.inc", &attrs), Some("C++"));
    }

    #[test]
    fn all_linguist_patterns_compile() {
        let r = rules();
        assert!(matches(&r.vendor, "third_party/x.c"));
        assert!(matches(&r.documentation, "Documentation/index.rst"));
    }
}
