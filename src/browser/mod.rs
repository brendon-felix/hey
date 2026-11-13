/* -------------------------------------------------------------------------- */
/*                               browser/mod.rs                               */
/* -------------------------------------------------------------------------- */

//! Full-screen lists of what a host keeps, to look through and change.

mod field;
pub mod memories;
pub mod threads;

use std::io::{IsTerminal, Write, stdout};
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::style::Print;
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use crossterm::{cursor, queue};
use yansi::Paint;

use crate::backend::Backend;
use crate::render::palette;

const REFRESH: Duration = Duration::from_secs(2);
/// Rows above and below the list: the title and filter, a gap and the keys.
const CHROME: usize = 4;

fn ensure_terminal(what: &str) -> Result<()> {
    if !stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("{what} needs a terminal");
    }
    Ok(())
}

/// What the last key did, shown in place of the keys until the next.
struct Note {
    text: String,
    failed: bool,
}

impl Note {
    fn said(text: String) -> Self {
        Self { text, failed: false }
    }

    fn failed(error: anyhow::Error) -> Self {
        Self {
            text: format!("{error:#}"),
            failed: true,
        }
    }
}

/// The screen's width and height.
fn size() -> Result<(usize, usize)> {
    Ok(terminal::size().map(|(width, height)| (usize::from(width), usize::from(height)))?)
}

/// How many rows of the list fit on the screen.
fn page() -> usize {
    let height = terminal::size().map_or(24, |(_, height)| usize::from(height));
    height.saturating_sub(CHROME).max(1)
}

/// The first of `rows` rows shown, moved from `scroll` just enough that the
/// row `at` is among them.
fn scrolled(scroll: usize, at: usize, rows: usize) -> usize {
    scroll.min(at).max((at + 1).saturating_sub(rows))
}

/// `view` and the host's name in bold at the left, `tallies` at the right.
fn header(backend: &Backend, view: &str, tallies: &[String], width: usize) -> String {
    let place = match backend.remote() {
        Some(remote) => format!(" on {remote}"),
        None => String::new(),
    };
    let title = format!(" {view}{place}");
    let tally_width: usize = tallies.iter().map(|tally| visible(tally)).sum::<usize>() + 3 * tallies.len().saturating_sub(1);
    let gap = width.saturating_sub(title.chars().count() + tally_width + 1);
    format!("{}{}{} ", title.bold(), " ".repeat(gap), tallies.join(" · "))
}

fn filter_line(filtering: bool, filter: &str) -> String {
    match (filtering, filter.is_empty()) {
        (true, _) => format!(" {} {filter}▏", palette().accent.paint("/")),
        (false, false) => format!(" {}", palette().muted.paint(&format!("/ {filter}"))),
        (false, true) => String::new(),
    }
}

/// The bottom line: what the last key did, else `keys`.
fn footer(note: Option<&Note>, keys: &str, width: usize) -> String {
    match note {
        Some(note) if note.failed => fit(&format!(" {}", note.text), width).red().to_string(),
        Some(note) => palette().muted.paint(&fit(&format!(" {}", note.text), width)).to_string(),
        None => palette().muted.paint(&fit(keys, width)).to_string(),
    }
}

/// A question in the footer that the next key answers.
fn question(text: &str, width: usize) -> String {
    fit(text, width).red().bold().to_string()
}

/// Draws `lines` from the top of a screen `height` rows tall, and `footer`
/// on its last row.
fn paint(mut lines: Vec<String>, footer: String, height: usize) -> Result<()> {
    lines.resize(height.saturating_sub(1), String::new());
    lines.push(footer);
    let mut out = stdout().lock();
    queue!(out, BeginSynchronizedUpdate)?;
    for (row, line) in lines.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16), Print(line), Clear(ClearType::UntilNewLine))?;
    }
    queue!(out, EndSynchronizedUpdate)?;
    out.flush()?;
    Ok(())
}

/// `text` on one line in at most `width` characters.
fn fit(text: &str, width: usize) -> String {
    let line: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    match line.chars().count() <= width {
        true => line,
        false => line.chars().take(width.saturating_sub(1)).chain(['…']).collect(),
    }
}

/// The width of `text` without its color codes.
fn visible(text: &str) -> usize {
    let mut width = 0;
    let mut escaped = false;
    for c in text.chars() {
        match (escaped, c) {
            (false, '\u{1b}') => escaped = true,
            (true, 'm') => escaped = false,
            (true, _) => {}
            (false, _) => width += 1,
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_rows_on_one_line() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("a long title", 6), "a lon…");
        assert_eq!(fit("two\nlines", 20), "two lines");
        assert_eq!(visible(&"new".blue().to_string()), 3);
    }
}
