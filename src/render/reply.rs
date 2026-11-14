/* -------------------------------------------------------------------------- */
/*                               render/reply.rs                              */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use yansi::Paint;

use super::highlight::Highlighter;
use super::theme::Theme;
use super::{Style, notice, screen, theme, wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Text,
    Reasoning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    Complete,
}

type Segment = (yansi::Style, Range<usize>);

/// A reply drawn as it arrives. Each complete line is highlighted with the
/// state the lines before it left; the open line is previewed with a copy of
/// that state, so text is coloured from its first character. Rows the open
/// line has wrapped past are printed for good, and only its last row is
/// left to redraw.
pub struct Reply {
    style: Style,
    width: usize,
    part: Option<Part>,
    highlighter: Option<Highlighter>,
    open: String,
    /// Rows of the open line already printed for good.
    printed: usize,
    /// The open line's last row as drawn, until more of the line arrives.
    tail: Option<String>,
}

impl Reply {
    pub fn new(style: &Style) -> Self {
        Self {
            style: style.clone(),
            width: style.width(),
            part: None,
            highlighter: None,
            open: String::new(),
            printed: 0,
            tail: None,
        }
    }

    pub fn text(&mut self, fragment: &str) {
        self.push(Part::Text, fragment);
    }

    pub fn push(&mut self, part: Part, fragment: &str) {
        if self.part != Some(part) {
            self.finish();
            self.begin(part);
        }
        for piece in fragment.split_inclusive('\n') {
            // A tab's width depends on its column, which wrapping cannot know.
            self.open.push_str(&piece.replace('\t', "    "));
            self.tail = None;
            if piece.ends_with('\n') {
                self.print(Stage::Complete);
            }
        }
    }

    /// Ends the open part, so whatever prints next starts on its own line.
    pub fn finish(&mut self) {
        if !self.open.is_empty() {
            self.open.push('\n');
            self.print(Stage::Complete);
        }
        self.part = None;
        self.highlighter = None;
    }

    /// The open line's last row, as far as it has arrived; the rows before
    /// it are printed for good.
    pub fn tail(&mut self) -> Option<String> {
        if self.tail.is_none() && !self.open.is_empty() {
            self.tail = self.print(Stage::Open);
        }
        self.tail.clone()
    }

    fn begin(&mut self, part: Part) {
        self.part = Some(part);
        self.width = self.style.width();
        self.highlighter = (part == Part::Text && self.style.highlight)
            .then(|| {
                Highlighter::new(theme::current())
                    .inspect_err(|error| notice(&format!("highlighting disabled: {error}")))
                    .ok()
            })
            .flatten();
        screen::line("");
    }

    /// Prints the open line's rows not yet printed for good: all of them once
    /// it is complete, else all but the last, which it returns to be drawn
    /// until the line grows.
    fn print(&mut self, stage: Stage) -> Option<String> {
        let part = self.part?;
        let line = match stage {
            Stage::Open => Cow::Borrowed(self.open.as_str()),
            Stage::Complete => Cow::Owned(std::mem::take(&mut self.open)),
        };
        let segments = segments(part, self.highlighter.as_mut(), &line, stage);
        let rows = wrap::rows(line.trim_end_matches('\n'), self.width);
        let mut painted = rows
            .iter()
            .skip(self.printed)
            .map(|row| paint(&line, &segments, &wrap::trimmed(&line, row)))
            .collect::<Vec<_>>();
        let tail = match stage {
            Stage::Open => {
                self.printed = rows.len() - 1;
                painted.pop()
            }
            Stage::Complete => {
                self.printed = 0;
                None
            }
        };
        painted.into_iter().for_each(screen::line);
        tail
    }
}

/// A whole reply highlighted with `theme`, as the rows [`Reply`] would print.
pub fn rows(text: &str, theme: Arc<Theme>, width: usize) -> Vec<String> {
    let mut highlighter = Highlighter::new(theme).ok();
    text.lines()
        .flat_map(|line| {
            let line = format!("{}\n", line.replace('\t', "    "));
            let segments = segments(Part::Text, highlighter.as_mut(), &line, Stage::Complete);
            wrap::rows(line.trim_end_matches('\n'), width)
                .iter()
                .map(|row| paint(&line, &segments, &wrap::trimmed(&line, row)))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn segments(
    part: Part,
    highlighter: Option<&mut Highlighter>,
    line: &str,
    stage: Stage,
) -> Vec<Segment> {
    let whole = |style| vec![(style, 0..line.len())];
    match (part, highlighter) {
        (Part::Reasoning, _) => whole(yansi::Style::new().dim().italic()),
        (Part::Text, None) => whole(yansi::Style::new()),
        (Part::Text, Some(highlighter)) => match stage {
            Stage::Open => highlighter.preview(line),
            Stage::Complete => highlighter.line(line),
        }
        .into_iter()
        .map(|(color, range)| (yansi::Style::new().fg(color.into()), range))
        .collect(),
    }
}

fn paint(line: &str, segments: &[Segment], row: &Range<usize>) -> String {
    segments
        .iter()
        .filter_map(|(style, span)| {
            let span = span.start.max(row.start)..span.end.min(row.end);
            (!span.is_empty()).then(|| line[span].paint(*style).to_string())
        })
        .collect()
}
