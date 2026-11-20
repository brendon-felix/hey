/* -------------------------------------------------------------------------- */
/*                                  table.rs                                  */
/* -------------------------------------------------------------------------- */

//! The `table` tool: draws a table with box-drawing characters and rounded
//! corners as Nushell does, its vertical borders aligned, cells wrapped to
//! fit the terminal's width. Hey prints Markdown as it is, so the model
//! can't count on a renderer to align a Markdown table.

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;
use textwrap::core::display_width;

/// Width assumed when the terminal's can't be found.
const FALLBACK_WIDTH: usize = 80;

pub struct Table;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableArgs {
    /// The column headers.
    headers: Vec<String>,
    /// The rows, each a list of cells in the headers' order.
    rows: Vec<Vec<String>>,
    /// Spaces on each side of a cell's text; default 1.
    padding: Option<usize>,
    /// The widest the table may be, in columns; default the terminal's width.
    width: Option<usize>,
}

impl Tool for Table {
    const NAME: &'static str = "table";
    type Args = TableArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Draw a table with box-drawing characters and rounded corners, its borders aligned \
         and its cells wrapped to the terminal's width. Always use it for tables rather than \
         writing Markdown tables, and put its output in your reply verbatim, in a code block."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<TableArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: TableArgs,
    ) -> Result<String, ToolExecutionError> {
        let width = args.width.unwrap_or_else(|| {
            crossterm::terminal::size()
                .map(|(columns, _)| usize::from(columns))
                .unwrap_or(FALLBACK_WIDTH)
        });
        Ok(render(
            &args.headers,
            &args.rows,
            args.padding.unwrap_or(1),
            width,
        ))
    }
}

/// The table as lines of text no wider than `width`, where it can be.
pub fn render(headers: &[String], rows: &[Vec<String>], padding: usize, width: usize) -> String {
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return String::new();
    }
    let cell = |row: &[String], at: usize| row.get(at).cloned().unwrap_or_default();
    let header: Vec<String> = (0..columns).map(|at| cell(headers, at)).collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| (0..columns).map(|at| cell(row, at)).collect())
        .collect();

    // Natural widths, then the widest columns shrunk until the table fits.
    let mut widths = vec![1usize; columns];
    for row in std::iter::once(&header).chain(&body) {
        for (at, text) in row.iter().enumerate() {
            let widest = text.lines().map(display_width).max().unwrap_or(0);
            widths[at] = widths[at].max(widest);
        }
    }
    let overhead = columns * (2 * padding + 1) + 1;
    let mut excess = (widths.iter().sum::<usize>() + overhead).saturating_sub(width);
    while excess > 0 {
        let widest = (0..columns).max_by_key(|&at| widths[at]).unwrap_or(0);
        if widths[widest] <= 1 {
            break;
        }
        widths[widest] -= 1;
        excess -= 1;
    }

    let rule = |left: &str, middle: &str, right: &str| {
        let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2 * padding)).collect();
        format!("{left}{}{right}", segments.join(middle))
    };
    let line = |row: &[String], centered: bool| -> Vec<String> {
        let wrapped: Vec<Vec<String>> = row
            .iter()
            .zip(&widths)
            .map(|(text, &w)| wrap(text, w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|at| {
                let cells: Vec<String> = wrapped
                    .iter()
                    .zip(&widths)
                    .map(|(lines, &w)| {
                        let text = lines.get(at).map(String::as_str).unwrap_or("");
                        let gap = w.saturating_sub(display_width(text));
                        let (before, after) = if centered {
                            (gap / 2, gap - gap / 2)
                        } else {
                            (0, gap)
                        };
                        let pad = " ".repeat(padding);
                        format!(
                            "{pad}{}{text}{}{pad}",
                            " ".repeat(before),
                            " ".repeat(after)
                        )
                    })
                    .collect();
                format!("│{}│", cells.join("│"))
            })
            .collect()
    };

    let mut out = vec![rule("╭", "┬", "╮")];
    out.extend(line(&header, true));
    out.push(rule("├", "┼", "┤"));
    for row in &body {
        out.extend(line(row, false));
    }
    out.push(rule("╰", "┴", "╯"));
    out.join("\n")
}

/// `text` in lines of at most `width` columns, long words broken.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let options = textwrap::Options::new(width).break_words(true);
    let lines: Vec<String> = text
        .lines()
        .flat_map(|line| {
            textwrap::wrap(line, &options)
                .into_iter()
                .map(|line| line.into_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borders_align() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = render(
            &s(&["name", "note"]),
            &[s(&["日本", "a long note that must wrap"]), s(&["x", "y"])],
            1,
            24,
        );
        let widths: Vec<_> = out.lines().map(display_width).collect();
        assert!(widths.iter().all(|&w| w == widths[0] && w <= 24), "{out}");
    }
}
