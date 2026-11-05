//! Markdown (GFM) to sanitized HTML.
//!
//! comrak renders with raw HTML allowed; ammonia then sanitizes everything,
//! including that raw HTML. Relative links and images (from Markdown or
//! raw HTML alike) are rewritten to point into the repository: links to the
//! blob view, images to the raw endpoint.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use comrak::adapters::SyntaxHighlighterAdapter;
use comrak::options::Plugins;
use comrak::{Options, markdown_to_html_with_plugins};

use super::{encode_bytes, escape_html, highlight, percent_decode};

/// Where relative URLs in a document resolve to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Links {
    /// URL of the repository, without a trailing slash (`/git/forks/foo`).
    pub repo_url: String,
    /// Revision as it appears in URLs (already percent-encoded).
    pub rev: String,
    /// Directory of the document inside the tree (no slashes at the ends;
    /// empty for the root).
    pub dir: Vec<u8>,
}

impl Links {
    /// Cache-key fragment identifying everything the output depends on.
    pub fn key(&self) -> String {
        format!(
            "{}\0{}\0{}",
            self.repo_url,
            self.rev,
            String::from_utf8_lossy(&self.dir)
        )
    }
}

/// Is this a Markdown file (by name)?
pub fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".md", ".markdown", ".mdown", ".mkd", ".mkdn"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

pub fn render(markdown: &str, links: &Links) -> String {
    let mut options = Options::default();
    let ext = &mut options.extension;
    ext.strikethrough = true;
    ext.table = true;
    ext.autolink = true;
    ext.tasklist = true;
    ext.footnotes = true;
    ext.alerts = true;
    ext.header_id_prefix = Some(String::new());
    ext.front_matter_delimiter = Some("---".into());
    options.render.r#unsafe = true;
    options.render.github_pre_lang = false;

    let mut plugins = Plugins::default();
    let highlighter = FenceHighlighter;
    plugins.render.codefence_syntax_highlighter = Some(&highlighter);
    let html = markdown_to_html_with_plugins(markdown, &options, &plugins);
    sanitize(&html, links.clone())
}

/// Plain text (non-Markdown READMEs) as an escaped `<pre>`.
pub fn render_plain(text: &str) -> String {
    let mut out = String::from("<pre class=\"plain\">");
    escape_html(&mut out, text);
    out.push_str("</pre>");
    out
}

fn sanitize(html: &str, links: Links) -> String {
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["input"])
        .add_tag_attributes("input", ["checked"])
        .set_tag_attribute_value("input", "type", "checkbox")
        .set_tag_attribute_value("input", "disabled", "")
        .add_generic_attributes(["id", "align"])
        .add_tag_attributes("a", ["aria-hidden"])
        .add_tag_attributes("img", ["title"])
        .add_tag_attributes("ol", ["start"])
        .add_tag_attributes("section", ["data-footnotes"])
        .add_tags(["section"])
        .link_rel(Some("noopener noreferrer nofollow"))
        .allowed_classes(allowed_classes())
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(move |element, attribute, value| {
            match (element, attribute) {
                ("a", "href") => rewrite(&links, value, "blob"),
                ("img", "src") => rewrite(&links, value, "raw"),
                _ => None,
            }
            .map_or(Some(Cow::Borrowed(value)), |v| Some(Cow::Owned(v)))
        });
    builder.clean(html).to_string()
}

fn allowed_classes() -> HashMap<&'static str, HashSet<&'static str>> {
    let set = |items: &[&'static str]| items.iter().copied().collect::<HashSet<_>>();
    let alerts = [
        "markdown-alert",
        "markdown-alert-note",
        "markdown-alert-tip",
        "markdown-alert-important",
        "markdown-alert-warning",
        "markdown-alert-caution",
    ];
    HashMap::from([
        ("span", set(&["hl-k", "hl-d", "hl-s", "hl-c", "hl-x"])),
        ("div", set(&alerts)),
        ("p", set(&["markdown-alert-title"])),
        ("a", set(&["anchor"])),
        ("section", set(&["footnotes"])),
        ("ul", set(&["contains-task-list"])),
        ("li", set(&["task-list-item"])),
    ])
}

/// `scheme:` per the URL spec, before any `/`, `?` or `#`.
fn has_scheme(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return false;
    };
    let scheme = &url[..colon];
    scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Rewrite a repository-relative URL; `None` leaves the value unchanged.
fn rewrite(links: &Links, url: &str, view: &str) -> Option<String> {
    if url.is_empty() || url.starts_with('#') || url.starts_with("//") || has_scheme(url) {
        return None;
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, f)) => (rest, Some(f)),
        None => (url, None),
    };
    let path = rest.split_once('?').map_or(rest, |(p, _)| p);
    let path = percent_decode(path);

    let mut parts: Vec<&[u8]> = Vec::new();
    if !path.starts_with(b"/") {
        parts.extend(links.dir.split(|b| *b == b'/').filter(|p| !p.is_empty()));
    }
    for part in path.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join(&b'/');
    let mut out = if joined.is_empty() {
        format!("{}/tree/{}", links.repo_url, links.rev)
    } else {
        format!(
            "{}/{view}/{}/{}",
            links.repo_url,
            links.rev,
            encode_bytes(&joined)
        )
    };
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    Some(out)
}

/// Highlights fenced code blocks with the same classes as the blob view.
struct FenceHighlighter;

impl SyntaxHighlighterAdapter for FenceHighlighter {
    fn write_highlighted(
        &self,
        output: &mut dyn fmt::Write,
        lang: Option<&str>,
        code: &str,
    ) -> fmt::Result {
        match lang.and_then(highlight::for_token) {
            Some(syntax) => output.write_str(&highlight::lines(code, syntax).join("\n")),
            None => {
                let mut escaped = String::with_capacity(code.len());
                escape_html(&mut escaped, code.trim_end_matches('\n'));
                output.write_str(&escaped)
            }
        }
    }

    fn write_pre_tag(
        &self,
        output: &mut dyn fmt::Write,
        _attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        output.write_str("<pre>")
    }

    fn write_code_tag(
        &self,
        output: &mut dyn fmt::Write,
        _attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        output.write_str("<code>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(dir: &str) -> Links {
        Links {
            repo_url: "/r".into(),
            rev: "main".into(),
            dir: dir.as_bytes().to_vec(),
        }
    }

    #[test]
    fn strips_dangerous_html() {
        let html = render(
            "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n\
             [x](javascript:alert(1)) <a href=\"data:text/html,hi\">d</a>\n\n\
             <iframe src=\"https://evil\"></iframe><style>body{}</style>\n\n\
             <div style=\"color:red\" onclick=\"x()\">s</div>",
            &links(""),
        );
        for bad in [
            "<script",
            "onerror",
            "javascript:",
            "data:text",
            "<iframe",
            "<style",
            "onclick",
            "style=",
        ] {
            assert!(!html.contains(bad), "{bad} survived: {html}");
        }
    }

    #[test]
    fn rewrites_relative_urls() {
        let html = render(
            "[a](docs/guide.md#install) [b](../up.md) [c](/root.md) [d](https://x.org/y) \
             [e](#frag) [f](./) ![i](img/logo.png) <img src=\"pic%20s.svg\"> \
             <a href=\"sub/\">raw</a>",
            &links("docs"),
        );
        assert!(
            html.contains(r#"href="/r/blob/main/docs/docs/guide.md#install""#),
            "{html}"
        );
        assert!(html.contains(r#"href="/r/blob/main/up.md""#), "{html}");
        assert!(html.contains(r#"href="/r/blob/main/root.md""#), "{html}");
        assert!(html.contains(r#"href="https://x.org/y""#), "{html}");
        assert!(html.contains(r##"href="#frag""##), "{html}");
        assert!(html.contains(r#"href="/r/blob/main/docs""#), "{html}");
        assert!(
            html.contains(r#"src="/r/raw/main/docs/img/logo.png""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="/r/raw/main/docs/pic%20s.svg""#),
            "{html}"
        );
        assert!(html.contains(r#"href="/r/blob/main/docs/sub""#), "{html}");
        // `..` can't escape the repository root.
        assert_eq!(
            rewrite(&links(""), "../../x", "blob").unwrap(),
            "/r/blob/main/x"
        );
        assert_eq!(rewrite(&links(""), "..", "blob").unwrap(), "/r/tree/main");
    }

    #[test]
    fn gfm_features_survive_sanitizing() {
        let html = render(
            "# Title\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n- [x] done\n- [ ] todo\n\n\
             > [!NOTE]\n> careful\n\nfoot[^1]\n\n[^1]: note\n\n```rust\nfn x() {}\n```\n",
            &links(""),
        );
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(html.contains("markdown-alert-note"), "{html}");
        assert!(html.contains(r#"id="title""#), "{html}");
        assert!(html.contains(r#"<span class="hl-k">fn</span>"#), "{html}");
        assert!(html.contains("footnote"), "{html}");
    }

    #[test]
    fn scheme_detection() {
        assert!(has_scheme("https://x"));
        assert!(has_scheme("mailto:a@b"));
        assert!(!has_scheme("a/b:c"));
        assert!(!has_scheme("x.md"));
        assert!(!has_scheme(":x"));
    }
}
