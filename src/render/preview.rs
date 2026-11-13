/* -------------------------------------------------------------------------- */
/*                              render/preview.rs                             */
/* -------------------------------------------------------------------------- */

use std::collections::VecDeque;

use rig_core::streaming::parse_partial_arguments;
use textwrap::core::display_width;
use yansi::{Paint, Style};

use super::tool::PREVIEW_LINES;
use super::{call_line, clip, palette, summarize, wrap};

/// Most rows of the text being written the box shows.
const ROWS: usize = 10;

/// What a [`Preview`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A command's output: its latest lines.
    Command,
    /// The model's reasoning, wrapped: its latest rows.
    Reasoning,
    /// What subagents do: their latest steps.
    Agents,
    /// The lines a call to `write` or `edit` removes and adds, marked `-`
    /// and `+`: the first of them.
    Changes,
    /// What a call gave back: its first lines.
    Returned,
}

impl Source {
    fn keeps_first(self) -> bool {
        matches!(self, Source::Changes | Source::Returned)
    }
}

/// Some of what the agent does besides answering, in a box: shown in place
/// as more arrives, and left in place once it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    source: Source,
    /// What the box's top border says.
    label: String,
    /// The lines it keeps, each with its `\n` once complete.
    lines: VecDeque<String>,
    /// How many lines it has been given.
    total: usize,
}

impl Preview {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            label: String::new(),
            lines: VecDeque::new(),
            total: 0,
        }
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn label(&mut self, label: String) {
        self.label = label;
    }

    pub fn push(&mut self, text: &str) {
        for piece in text.split_inclusive('\n') {
            match self.lines.back_mut() {
                Some(open) if !open.ends_with('\n') => open.push_str(piece),
                _ => {
                    self.lines.push_back(piece.to_owned());
                    self.total += 1;
                }
            }
            // Reasoning spaces its paragraphs, which would waste a preview's rows.
            if self.source == Source::Reasoning
                && self.lines.back().is_some_and(|line| line.ends_with('\n') && blank(line))
            {
                self.lines.pop_back();
                self.total -= 1;
            }
        }
        if self.source.keeps_first() {
            self.lines.truncate(PREVIEW_LINES);
        } else {
            let excess = self.lines.len().saturating_sub(PREVIEW_LINES);
            self.lines.drain(..excess);
        }
    }

    /// The box, `width` columns wide. Its lower border says how many lines
    /// it leaves out, then `outcome`. Reasoning has a row to itself above
    /// it, as text the reply begins.
    pub fn framed(&self, width: usize, outcome: &str) -> Vec<String> {
        let bottom = [self.left_out(), outcome.to_owned()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let framed = framed(&self.label, self.rows(inner(width)), &bottom, width);
        match self.source {
            Source::Reasoning => std::iter::once(String::new()).chain(framed).collect(),
            _ => framed,
        }
    }

    /// How many of the lines it was given it leaves out, where they are
    /// counted: reasoning's rows are wrapped, and subagents' steps go on.
    fn left_out(&self) -> String {
        let more = match self.source {
            Source::Command | Source::Changes | Source::Returned => self.total - self.lines.len(),
            Source::Reasoning | Source::Agents => 0,
        };
        match more {
            0 => String::new(),
            1 => "1 more line".to_owned(),
            more => format!("{more} more lines"),
        }
    }

    /// At most [`PREVIEW_LINES`] rows, none wider than `width` columns.
    fn rows(&self, width: usize) -> Vec<Row> {
        let lines = self.lines.iter().map(|line| line.strip_suffix('\n').unwrap_or(line));
        let lines = lines.map(|line| line.replace('\t', "    "));
        let mut rows: Vec<Row> = match self.source {
            Source::Command | Source::Returned => {
                lines.map(|line| Row::new(&[(&clip(&line, width), Style::new())])).collect()
            }
            Source::Changes => lines.map(|line| change(&line, width)).collect(),
            Source::Agents => lines.map(|line| step(&line, width)).collect(),
            Source::Reasoning => lines
                .filter(|line| !blank(line))
                .flat_map(|line| reasoning(&line, width))
                .collect(),
        };
        rows.drain(..rows.len().saturating_sub(PREVIEW_LINES));
        rows
    }
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn change(line: &str, width: usize) -> Row {
    let style = match line.as_bytes().first() {
        Some(b'+') => Style::new().green(),
        Some(b'-') => Style::new().red(),
        _ => Style::new(),
    };
    Row::new(&[(&clip(line, width), style)])
}

/// A subagent's step, its label muted, as [`super::agent_step`] puts it.
fn step(line: &str, width: usize) -> Row {
    let line = clip(line, width);
    let muted = Style::new().fg(palette().muted.into());
    match line.split_once(" › ") {
        Some((label, step)) => Row::new(&[(label, muted), (" › ", muted), (step, Style::new())]),
        None => Row::new(&[(&line, Style::new())]),
    }
}

fn reasoning(line: &str, width: usize) -> Vec<Row> {
    wrap::rows(line, width)
        .into_iter()
        .map(|range| Row::new(&[(&line[wrap::trimmed(line, &range)], Style::new().italic())]))
        .collect()
}

/// A row of a box: its text as painted, and the columns it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    painted: String,
    width: usize,
}

impl Row {
    fn new(parts: &[(&str, Style)]) -> Self {
        Self {
            painted: parts.iter().map(|(text, style)| text.paint(*style).to_string()).collect(),
            width: parts.iter().map(|(text, _)| display_width(text)).sum(),
        }
    }
}

/// Columns inside a box `width` columns wide: it is indented two, and has
/// a border and a space on each side.
fn inner(width: usize) -> usize {
    width.saturating_sub(6).max(1)
}

/// `rows` in a box `width` columns wide, its borders muted, with `top` and
/// `bottom` set in them.
pub fn framed(top: &str, rows: impl IntoIterator<Item = Row>, bottom: &str, width: usize) -> Vec<String> {
    let inner = inner(width);
    let muted = palette().muted;
    let border = |left: &str, label: &str, right: &str| {
        let label = match label.is_empty() || inner < 4 {
            true => String::new(),
            false => format!(" {} ", clip(label, inner - 2)),
        };
        let rule = "─".repeat((inner + 1).saturating_sub(display_width(&label)));
        format!("  {}", muted.paint(&format!("{left}─{label}{rule}{right}")))
    };
    let side = muted.paint("│");
    std::iter::once(border("╭", top, "╮"))
        .chain(rows.into_iter().map(|row| {
            let padding = " ".repeat(inner.saturating_sub(row.width));
            format!("  {side} {}{padding} {side}", row.painted)
        }))
        .chain(std::iter::once(border("╰", bottom, "╯")))
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
    let text = text.replace('\t', "    ").replace('\r', "");
    let rows: Vec<&str> = text
        .lines()
        .flat_map(|line| {
            wrap::rows(line, inner(width))
                .into_iter()
                .map(move |row| &line[wrap::trimmed(line, &row)])
        })
        .collect();
    let shown = &rows[rows.len().saturating_sub(height)..];
    let label = match shown.len() < rows.len() {
        true => format!("{} lines", text.lines().count()),
        false => String::new(),
    };
    let shown = shown.iter().map(|row| Row::new(&[(row, Style::new())]));
    framed(&label, shown, "", width)
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
            preview.framed(18, ""),
            [
                "",
                "  ╭──────────────╮",
                "  │ five six     │",
                "  │ seven        │",
                "  │ eight        │",
                "  │ nine         │",
                "  │ ten          │",
                "  ╰──────────────╯",
            ]
        );
    }

    #[test]
    fn previews_the_first_lines_given_back_and_counts_the_rest_below() {
        yansi::disable();
        let mut preview = Preview::new(Source::Returned);
        preview.label("read a.rs".into());
        preview.push("1\n2\n3\n4\n5\n6\n7");
        let framed = preview.framed(22, "");
        assert_eq!(
            framed,
            [
                "  ╭─ read a.rs ──────╮",
                "  │ 1                │",
                "  │ 2                │",
                "  │ 3                │",
                "  │ 4                │",
                "  │ 5                │",
                "  ╰─ 2 more lines ───╯",
            ]
        );
        assert!(framed.iter().all(|row| display_width(row) == 22));
        let mut changes = Preview::new(Source::Changes);
        changes.push("+ a\n");
        let bottom = changes.framed(30, "wrote 1 line").pop().unwrap();
        assert!(bottom.starts_with("  ╰─ wrote 1 line ─") && display_width(&bottom) == 30);
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
