//! Syntax highlighting into monochrome CSS classes.
//!
//! The whole file is parsed in one pass (so multi-line strings and comments
//! are right), but the output is split into lines whose `<span>`s are
//! balanced: each line can be put in its own table row.
//!
//! Scopes collapse to a handful of classes (see `syntax.css`):
//! `hl-k` keywords/storage, `hl-d` names/numbers/constants, `hl-s`
//! strings/punctuation/operators, `hl-c` comments, `hl-x` invalid.

use std::sync::{Arc, OnceLock};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use super::escape_html;
use crate::cache::{Cache, Key};
use crate::git::ObjectId;

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

/// The embedded syntax set (bat's ~200 syntaxes). Loading takes a few tens
/// of milliseconds; call once at startup to keep it off the request path.
pub fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// Pick a syntax from the file name, falling back to the first line
/// (shebangs, modelines). `None` means plain text.
pub fn detect(path: &str, first_line: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let name = path.rsplit('/').next().unwrap_or(path);
    let found = set
        .find_syntax_by_extension(name)
        .or_else(|| {
            let (_, ext) = name.rsplit_once('.')?;
            set.find_syntax_by_extension(ext)
                .or_else(|| set.find_syntax_by_extension(&ext.to_ascii_lowercase()))
        })
        .or_else(|| set.find_syntax_by_first_line(first_line));
    found.filter(|s| s.name != "Plain Text")
}

/// Syntax for a Markdown code fence info string (`rust`, `sh`, `py`, …).
pub fn for_token(token: &str) -> Option<&'static SyntaxReference> {
    let token = token.split([' ', ',', '{']).next()?.trim();
    if token.is_empty() {
        return None;
    }
    syntaxes()
        .find_syntax_by_token(token)
        .filter(|s| s.name != "Plain Text")
}

struct Selectors {
    comment: Scope,
    invalid: Scope,
    string: Scope,
    punctuation: Scope,
    operator: Scope,
    keyword: Scope,
    storage: Scope,
    display: [Scope; 6],
}

fn selectors() -> &'static Selectors {
    static SELECTORS: OnceLock<Selectors> = OnceLock::new();
    SELECTORS.get_or_init(|| {
        let s = |name: &str| Scope::new(name).expect("valid scope");
        Selectors {
            comment: s("comment"),
            invalid: s("invalid"),
            string: s("string"),
            punctuation: s("punctuation"),
            operator: s("keyword.operator"),
            keyword: s("keyword"),
            storage: s("storage"),
            display: [
                s("entity.name"),
                s("support.function"),
                s("support.type"),
                s("support.class"),
                s("constant.numeric"),
                s("constant.language"),
            ],
        }
    })
}

/// Class for text under `stack`: comments win outright, otherwise the
/// innermost scope with a class decides.
fn class_for(stack: &[Scope]) -> Option<&'static str> {
    let sel = selectors();
    if stack.iter().any(|s| sel.comment.is_prefix_of(*s)) {
        return Some("hl-c");
    }
    for &scope in stack.iter().rev() {
        if sel.invalid.is_prefix_of(scope) {
            return Some("hl-x");
        }
        if sel.operator.is_prefix_of(scope)
            || sel.punctuation.is_prefix_of(scope)
            || sel.string.is_prefix_of(scope)
        {
            return Some("hl-s");
        }
        if sel.keyword.is_prefix_of(scope) || sel.storage.is_prefix_of(scope) {
            return Some("hl-k");
        }
        if sel.display.iter().any(|d| d.is_prefix_of(scope)) {
            return Some("hl-d");
        }
    }
    None
}

/// Appends text to a line, opening/closing spans only when the class changes.
struct LineWriter {
    out: String,
    open: Option<&'static str>,
}

impl LineWriter {
    fn push(&mut self, text: &str, class: Option<&'static str>) {
        let text = text.trim_end_matches(['\n', '\r']);
        if text.is_empty() {
            return;
        }
        if class != self.open {
            if self.open.is_some() {
                self.out.push_str("</span>");
            }
            if let Some(class) = class {
                self.out.push_str("<span class=\"");
                self.out.push_str(class);
                self.out.push_str("\">");
            }
            self.open = class;
        }
        escape_html(&mut self.out, text);
    }

    fn finish(mut self) -> String {
        if self.open.is_some() {
            self.out.push_str("</span>");
        }
        self.out
    }
}

/// Highlight `text` and return one HTML fragment per line.
///
/// If the grammar fails part-way, the remaining lines are emitted plain.
pub fn lines(text: &str, syntax: &SyntaxReference) -> Vec<String> {
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut failed = false;
    let mut out = Vec::new();
    for line in LinesWithEndings::from(text) {
        let ops = if failed {
            None
        } else {
            state.parse_line(line, set).ok()
        };
        let Some(ops) = ops else {
            failed = true;
            out.push(plain_line(line));
            continue;
        };
        let mut writer = LineWriter {
            out: String::with_capacity(line.len() * 2),
            open: None,
        };
        let mut pos = 0;
        for (at, op) in ops {
            let at = at.min(line.len());
            if at > pos {
                writer.push(&line[pos..at], class_for(stack.as_slice()));
                pos = at;
            }
            if stack.apply(&op).is_err() {
                failed = true;
            }
        }
        writer.push(&line[pos..], class_for(stack.as_slice()));
        out.push(writer.finish());
    }
    out
}

/// Lines longer than this (minified code) make syntect crawl; such files
/// are shown plain.
const MAX_LINE_BYTES: usize = 5000;

/// HTML lines of blob `oid` (the file at `path`, contents `text`),
/// highlighted when a syntax is detected, the blob is at most `max_bytes`
/// and no line is minified-long, plain otherwise. Highlighted lines are cached by blob and
/// shared by every view of it. Also returns the detected language.
pub fn cached_lines(
    cache: &Cache,
    oid: ObjectId,
    path: &str,
    text: &str,
    max_bytes: u64,
) -> (Arc<Vec<String>>, Option<&'static str>) {
    let first_line = text.lines().next().unwrap_or_default();
    let syntax = detect(path, first_line);
    let language = syntax.map(|s| s.name.as_str());
    let lines = match syntax {
        Some(syntax)
            if text.len() as u64 <= max_bytes
                && !text.lines().any(|l| l.len() > MAX_LINE_BYTES) =>
        {
            let key = Key::Highlight {
                blob: oid,
                syntax: syntax.name.clone(),
            };
            cache
                .lines(key, || {
                    Ok::<_, std::convert::Infallible>(lines(text, syntax))
                })
                .unwrap_or_else(|never| match never {})
        }
        _ => Arc::new(plain_lines(text)),
    };
    (lines, language)
}

/// Escaped lines without highlighting.
pub fn plain_lines(text: &str) -> Vec<String> {
    LinesWithEndings::from(text).map(plain_line).collect()
}

fn plain_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    escape_html(&mut out, line.trim_end_matches(['\n', '\r']));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_name_extension_and_first_line() {
        assert_eq!(detect("src/main.rs", "").unwrap().name, "Rust");
        assert_eq!(detect("Makefile", "").unwrap().name, "Makefile");
        assert_eq!(detect("a/b.PY", "").unwrap().name, "Python");
        assert_eq!(
            detect("run", "#!/bin/bash").unwrap().name,
            "Bourne Again Shell (bash)"
        );
        assert!(detect("notes.txt", "hello").is_none());
        assert_eq!(for_token("rust").unwrap().name, "Rust");
        assert_eq!(for_token("sh").unwrap().name, "Bourne Again Shell (bash)");
        assert!(for_token("").is_none());
    }

    #[test]
    fn spans_are_balanced_per_line_across_multiline_comments() {
        let src = "fn main() {\n    /* a\n    b */ let x = \"<s>\";\n}\n";
        let lines = lines(src, detect("x.rs", "").unwrap());
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(
                line.matches("<span").count(),
                line.matches("</span>").count(),
                "{line}"
            );
        }
        assert!(
            lines[0].contains(r#"<span class="hl-k">fn</span>"#),
            "{}",
            lines[0]
        );
        // The comment's second line is still a comment.
        assert!(
            lines[2].starts_with(r#"<span class="hl-c">    b */"#),
            "{}",
            lines[2]
        );
        // Escaped string contents.
        assert!(lines[2].contains("&lt;s&gt;"), "{}", lines[2]);
        assert!(!lines.iter().any(|l| l.contains('\n')));
    }

    #[test]
    fn plain_lines_escape() {
        assert_eq!(plain_lines("a<b\r\n\nc"), vec!["a&lt;b", "", "c"]);
    }
}
