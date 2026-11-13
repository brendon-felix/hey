/* -------------------------------------------------------------------------- */
/*                               render/tool.rs                               */
/* -------------------------------------------------------------------------- */

use rig_core::message::ToolResult;
use rig_tools::{EditArgs, WriteArgs, split_status};
use yansi::Paint;

use super::{clip, palette, terminal_width};
use crate::config::ToolOutput;

/// Lines a preview keeps of what a call wrote or printed.
pub(super) const PREVIEW_LINES: usize = 5;

/// What a call to `write` or `edit` changes in its file.
pub(super) fn changes(name: &str, args: &str, shown: ToolOutput) {
    match name {
        "write" => {
            if let Ok(write) = serde_json::from_str::<WriteArgs>(args) {
                let lines: Vec<_> = write.content.lines().map(|line| (Mark::Added, line)).collect();
                excerpt(&lines, shown, Keep::First);
            }
        }
        "edit" => {
            if let Ok(edit) = serde_json::from_str::<EditArgs>(args) {
                let lines: Vec<_> = edit
                    .edits
                    .iter()
                    .flat_map(|edit| diff(&edit.old_text, &edit.new_text))
                    .collect();
                excerpt(&lines, shown, Keep::First);
            }
        }
        _ => {}
    }
}

/// What a finished command printed, as it showed while it ran.
pub(super) fn printed(result: &ToolResult, shown: ToolOutput) {
    if result.name.as_str() != "shell" || result.is_error {
        return;
    }
    let text: Vec<_> = result.content.iter().filter_map(|content| content.as_text()).collect();
    let text = text.join("\n");
    let lines: Vec<_> = split_status(&text).0.lines().map(|line| (Mark::Printed, line)).collect();
    excerpt(&lines, shown, Keep::Last);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Printed,
    Added,
    Removed,
}

/// Which end of the lines a preview keeps.
#[derive(Debug, Clone, Copy)]
enum Keep {
    First,
    Last,
}

/// The lines an edit removes and adds, without those it keeps at either end.
fn diff<'a>(old: &'a str, new: &'a str) -> Vec<(Mark, &'a str)> {
    let old: Vec<_> = old.lines().collect();
    let new: Vec<_> = new.lines().collect();
    let same = |(a, b): (&&str, &&str)| a == b;
    let prefix = old.iter().zip(&new).take_while(|&pair| same(pair)).count();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old.iter().rev().zip(new.iter().rev()).take_while(|&pair| same(pair)).count();
    let removed = old[..old.len() - suffix].iter().map(|line| (Mark::Removed, *line));
    let added = new[..new.len() - suffix].iter().map(|line| (Mark::Added, *line));
    removed.chain(added).collect()
}

fn excerpt(lines: &[(Mark, &str)], shown: ToolOutput, keep: Keep) {
    match shown {
        ToolOutput::Hidden => {}
        ToolOutput::Full => lines.iter().for_each(|&(mark, text)| println!("{}", line(mark, text))),
        ToolOutput::Preview => {
            let more = lines.len().saturating_sub(PREVIEW_LINES);
            let (kept, before, after) = match keep {
                Keep::First => (&lines[..lines.len() - more], 0, more),
                Keep::Last => (&lines[more..], more, 0),
            };
            elided(before);
            kept.iter().for_each(|&(mark, text)| println!("{}", row(mark, text)));
            elided(after);
        }
    }
}

fn elided(lines: usize) {
    if lines > 0 {
        let muted = palette().muted;
        println!("  {}", muted.paint(&format!("… {lines} more lines")));
    }
}

/// A line under a call: what it printed, or what it writes to a file.
pub(super) fn line(mark: Mark, text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let muted = palette().muted;
    match mark {
        Mark::Printed => format!("  {} {}", muted.paint("│"), muted.paint(text)),
        Mark::Added => format!("  {}", format!("+ {text}").green()),
        Mark::Removed => format!("  {}", format!("- {text}").red()),
    }
}

/// [`line`] cut to a single row of the terminal.
pub(super) fn row(mark: Mark, text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text).replace('\t', "    ");
    line(mark, &clip(&text, terminal_width().saturating_sub(4)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_leave_out_the_lines_an_edit_keeps() {
        use Mark::{Added, Removed};
        assert_eq!(diff("a\nb\nc", "a\nB\nc"), [(Removed, "b"), (Added, "B")]);
        assert_eq!(diff("fn f() {\n}", "fn f() {\n    x\n}"), [(Added, "    x")]);
        assert_eq!(diff("a\na", "a"), [(Removed, "a")]);
        assert_eq!(diff("same", "same"), []);
    }
}
