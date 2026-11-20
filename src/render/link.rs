//! Clickable links: URLs and paths that exist are wrapped in OSC 8
//! hyperlinks (`ESC ] 8 ; ; URI ST text ESC ] 8 ; ; ST`), the sequence
//! nushell's `ansi link` writes. Terminals without support ignore it.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://[^\s\x1b<>\x22'`]+|[~.\w@+\-]*(?:/[\w.@+\-]+)+/?|[\w@+\-][\w.@+\-]*\.\w+",
    )
    .expect("valid pattern")
});
static ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b(?:\[[0-9;:?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))").expect("valid pattern"));

/// `text` with its URLs and existing paths made clickable. Existing
/// hyperlinks and styling are left as they are.
pub fn linkify(text: &str) -> String {
    if text.contains("\x1b]8;") || !text.contains(['/', '.']) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for escape in ESCAPE.find_iter(text) {
        plain(&text[at..escape.start()], &mut out);
        out.push_str(escape.as_str());
        at = escape.end();
    }
    plain(&text[at..], &mut out);
    out
}

fn plain(text: &str, out: &mut String) {
    let mut at = 0;
    for found in TOKEN.find_iter(text) {
        let before = text[..found.start()].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '/' | ':' | '_')) {
            continue;
        }
        let token = found.as_str();
        let (token, uri) = if token.starts_with("http") {
            let token = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
            (token, Some(token.to_owned()))
        } else {
            let token = token.trim_end_matches('.');
            (token, file_uri(token))
        };
        let Some(uri) = uri else { continue };
        out.push_str(&text[at..found.start()]);
        out.push_str(&format!("\x1b]8;;{uri}\x1b\\{token}\x1b]8;;\x1b\\"));
        at = found.start() + token.len();
    }
    out.push_str(&text[at..]);
}

/// The `file://` URI of `token` when it names something that exists.
fn file_uri(token: &str) -> Option<String> {
    let path: PathBuf = match token.strip_prefix('~') {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest.trim_start_matches('/')),
        None => std::env::current_dir().ok()?.join(token),
    };
    let path = std::fs::canonicalize(&path).ok().filter(|_| Path::new(&path).exists())?;
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_urls_and_leaves_styling() {
        let linked = linkify("\x1b[2msee https://example.com/a.\x1b[0m");
        assert_eq!(
            linked,
            "\x1b[2msee \x1b]8;;https://example.com/a\x1b\\https://example.com/a\x1b]8;;\x1b\\.\x1b[0m"
        );
    }

    #[test]
    fn links_existing_paths_only() {
        assert!(linkify("/tmp").contains("]8;;file:///tmp"));
        assert_eq!(linkify("/no/such/path"), "/no/such/path");
    }
}
