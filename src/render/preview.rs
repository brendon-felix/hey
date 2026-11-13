/* -------------------------------------------------------------------------- */
/*                              render/preview.rs                             */
/* -------------------------------------------------------------------------- */

use rig_core::streaming::parse_partial_arguments;
use textwrap::core::display_width;

use super::{call_line, palette, summarize, wrap};

/// Most rows of the text being written the box shows.
const ROWS: usize = 10;

/// A tool call as far as the model has written it, drawn as it grows: the
/// line it prints once complete, over a box with the end of the text the
/// model is writing into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    name: String,
    /// Its arguments' raw JSON so far.
    args: String,
}

impl Call {
    pub fn new(name: String) -> Self {
        Self {
            name,
            args: String::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn push(&mut self, json: &str) {
        self.args.push_str(json);
    }

    /// At most `height` rows, none wider than `width` columns, starting
    /// with a blank one as the line the call prints does.
    pub fn rows(&self, width: usize, height: usize) -> Vec<String> {
        let summary = summarize(&parse_partial_arguments(&self.args));
        let mut rows = vec![String::new(), call_line(&self.name, &summary, width)];
        if let Some(text) = writing(&self.args).filter(|text| text.contains('\n')) {
            let room = height.saturating_sub(rows.len() + 2);
            rows.extend(boxed(&text, width, room.min(ROWS)));
        }
        rows.truncate(height);
        rows
    }
}

/// `text` in a box `width` columns wide, its last `height` rows at most.
fn boxed(text: &str, width: usize, height: usize) -> Vec<String> {
    // Indented two columns, and a border and a space on each side.
    let inner = width.saturating_sub(6).max(1);
    let text = text.replace('\t', "    ").replace('\r', "");
    let rows: Vec<&str> = text
        .lines()
        .flat_map(|line| {
            wrap::rows(line, inner)
                .into_iter()
                .map(move |row| &line[wrap::trimmed(line, &row)])
        })
        .collect();
    let shown = &rows[rows.len().saturating_sub(height)..];
    let label = Some(format!(" {} lines ", text.lines().count()))
        .filter(|label| shown.len() < rows.len() && display_width(label) <= inner + 1)
        .unwrap_or_default();
    let rule = |count: usize| "─".repeat(count);
    let muted = palette().muted;
    let top = format!("╭─{label}{}╮", rule((inner + 1).saturating_sub(display_width(&label))));
    let bottom = format!("╰{}╯", rule(inner + 2));
    std::iter::once(format!("  {}", muted.paint(&top)))
        .chain(shown.iter().map(|row| {
            let padding = " ".repeat(inner.saturating_sub(display_width(row)));
            format!("  {} {row}{padding} {}", muted.paint("│"), muted.paint("│"))
        }))
        .chain(std::iter::once(format!("  {}", muted.paint(&bottom))))
        .collect()
}

/// The text of the string value raw JSON arguments end in, as far as it
/// has arrived, or else of the last one complete: what the model is
/// writing, or wrote last.
fn writing(json: &str) -> Option<String> {
    // Where the last value began, and where it ended once it has.
    let mut value: Option<(usize, Option<usize>)> = None;
    let mut string: Option<bool> = None;
    let (mut escaped, mut previous) = (false, None);
    for (at, c) in json.char_indices() {
        match string {
            Some(is_value) => match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => {
                    string = None;
                    previous = Some(c);
                    if let (true, Some((start, _))) = (is_value, value) {
                        value = Some((start, Some(at + 1)));
                    }
                }
                _ => {}
            },
            None if c == '"' => {
                let is_value = previous == Some(':');
                if is_value {
                    value = Some((at, None));
                }
                string = Some(is_value);
            }
            None if !c.is_whitespace() => previous = Some(c),
            None => {}
        }
    }
    let (start, end) = value?;
    let literal = match end {
        Some(end) => json[start..end].to_owned(),
        None => format!("{}\"", complete_escapes(&json[start..])),
    };
    serde_json::from_str(&literal).ok()
}

/// `literal` without an escape it ends partway through.
fn complete_escapes(literal: &str) -> &str {
    let backslashes = literal.len() - literal.trim_end_matches('\\').len();
    if backslashes % 2 == 1 {
        return &literal[..literal.len() - 1];
    }
    match literal.rfind("\\u") {
        Some(at) if literal.len() - at < 6 => &literal[..at],
        _ => literal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_value_being_written_or_written_last() {
        let cases = [
            (r#"{"path": "a.rs", "content": "fn main() {\n    pri"#, Some("fn main() {\n    pri")),
            (r#"{"path": "a.rs", "cont"#, Some("a.rs")),
            (r#"{"content": "say \"hi\"\"#, Some("say \"hi\"")),
            (r#"{"content": "caf\u00e9 \u00"#, Some("café ")),
            (r#"{"edits": [{"old_text": "a", "new_text": "b\nc"}]}"#, Some("b\nc")),
            (r#"{"path""#, None),
        ];
        for (json, expected) in cases {
            assert_eq!(writing(json).as_deref(), expected, "{json}");
        }
    }

    #[test]
    fn boxes_the_last_rows_within_the_width() {
        yansi::disable();
        let rows = boxed("one\ntwo\nthree", 20, 2);
        assert_eq!(
            rows,
            [
                "  ╭─ 3 lines ──────╮",
                "  │ two            │",
                "  │ three          │",
                "  ╰────────────────╯",
            ]
        );
        assert!(rows.iter().all(|row| display_width(row) == 20));
        assert!(boxed("a\nb\nc", 8, 1).iter().all(|row| display_width(row) == 8));
    }
}
