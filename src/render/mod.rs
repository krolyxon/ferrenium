//! Presentation helpers shared by handlers and templates.

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod time;

use std::fmt::Write;

/// Percent-encode a slash-separated path for use in a URL, keeping `/`.
pub fn encode_path(path: &str) -> String {
    encode_bytes(path.as_bytes())
}

/// [`encode_path`] for paths that may not be UTF-8 (git tree paths).
pub fn encode_bytes(path: &[u8]) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Decode `%XX` escapes (invalid escapes are kept literally).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Append `text` to `out` with `&<>"'` escaped.
pub fn escape_html(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// "12.4 KB" (binary multiples, conventional labels).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%2Fc"), b"a b/c");
        assert_eq!(percent_decode("%C3%BC"), "ü".as_bytes());
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(12_698), "12.4 KB");
        assert_eq!(human_size(300 * 1024), "300 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn escaping() {
        let mut s = String::new();
        escape_html(&mut s, "<a href=\"x\">&'");
        assert_eq!(s, "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn encodes_reserved_but_keeps_slashes() {
        assert_eq!(encode_path("forks/foo.rs"), "forks/foo.rs");
        assert_eq!(encode_path("a b#?%"), "a%20b%23%3F%25");
        assert_eq!(encode_path("ünï"), "%C3%BCn%C3%AF");
    }
}
