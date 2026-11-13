/* -------------------------------------------------------------------------- */
/*                              render/preview.rs                             */
/* -------------------------------------------------------------------------- */

use std::collections::VecDeque;
use std::time::Duration;

use rig_core::streaming::parse_partial_arguments;
use textwrap::core::display_width;

use super::tool::{Mark, PREVIEW_LINES, row};
use super::{agent_row, call_line, palette, summarize, thought_text, wrap};

/// Most rows of the text being written the box shows.
const ROWS: usize = 10;

/// What a [`Preview`] shows the latest of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A running command's output, a row per line.
    Command,
    /// The model's reasoning, wrapped, under a line saying it thinks.
    Reasoning,
    /// What subagents do, a row per step.
    Agents,
}

/// Whether what a [`Preview`] shows goes on, and once it has ended, how long
/// it took when that is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Going,
    Ended(Option<Duration>),
}

/// The latest of what the agent does besides answering, which shows in
/// place as more arrives and stays once it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    source: Source,
    /// Its latest lines, each with its `\n` once complete.
    lines: VecDeque<String>,
}

impl Preview {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            lines: VecDeque::new(),
        }
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn push(&mut self, text: &str) {
        for piece in text.split_inclusive('\n') {
            match self.lines.back_mut() {
                Some(open) if !open.ends_with('\n') => open.push_str(piece),
                _ => self.lines.push_back(piece.to_owned()),
            }
            // Reasoning spaces its paragraphs, which would waste a preview's rows.
            if self.source == Source::Reasoning
                && self.lines.back().is_some_and(|line| line.ends_with('\n') && blank(line))
            {
                self.lines.pop_back();
            }
        }
        let excess = self.lines.len().saturating_sub(PREVIEW_LINES);
        self.lines.drain(..excess);
    }

    /// Its rows, none wider than `width` columns: at most [`PREVIEW_LINES`]
    /// of the latest, under a header for reasoning.
    pub fn rows(&self, width: usize, progress: Progress) -> Vec<String> {
        let lines = self.lines.iter().map(|line| line.strip_suffix('\n').unwrap_or(line));
        let mut rows: Vec<String> = match self.source {
            Source::Command => lines.map(|line| row(Mark::Printed, line, width)).collect(),
            Source::Agents => lines.map(|line| agent_row(line, width)).collect(),
            Source::Reasoning => lines
                .filter(|line| !blank(line))
                .flat_map(|line| reasoning_rows(line, width))
                .collect(),
        };
        rows.drain(..rows.len().saturating_sub(PREVIEW_LINES));
        match self.source {
            Source::Reasoning => {
                let header = match progress {
                    Progress::Going => "Thinking".to_owned(),
                    Progress::Ended(took) => thought_text(took),
                };
                let header = palette().muted.paint(&header).italic().to_string();
                [String::new(), header].into_iter().chain(rows).collect()
            }
            Source::Command | Source::Agents => rows,
        }
    }
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// A line of reasoning wrapped beside the preview's border.
fn reasoning_rows(line: &str, width: usize) -> Vec<String> {
    let line = line.replace('\t', "    ");
    let muted = palette().muted;
    wrap::rows(&line, width.saturating_sub(4))
        .into_iter()
        .map(|range| {
            let text = &line[wrap::trimmed(&line, &range)];
            format!("  {} {}", muted.paint("│"), muted.paint(text).italic())
        })
        .collect()
}

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
    /// with a blank one as the line the call prints does. The line names
    /// the call by its one-line arguments; longer ones show in the box.
    pub fn rows(&self, width: usize, height: usize) -> Vec<String> {
        let mut args = parse_partial_arguments(&self.args);
        args.retain(|_, value| !value.as_str().is_some_and(|text| text.contains('\n')));
        let summary = match args.is_empty() {
            true => String::new(),
            false => summarize(&args),
        };
        let mut rows = vec![String::new(), call_line(&self.name, None, &summary, width)];
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
    fn previews_the_latest_reasoning_wrapped_without_its_gaps() {
        yansi::disable();
        let mut preview = Preview::new(Source::Reasoning);
        for piece in ["one\n\ntwo\n", "\nthree four five six", " seven\n  \n", "eight\nnine\nten"] {
            preview.push(piece);
        }
        assert_eq!(
            preview.rows(16, Progress::Going),
            [
                "",
                "Thinking",
                "  │ five six",
                "  │ seven",
                "  │ eight",
                "  │ nine",
                "  │ ten",
            ]
        );
        let ended = preview.rows(16, Progress::Ended(Some(Duration::from_secs(72))));
        assert_eq!(ended[1], "Thought for 1m 12s");
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
