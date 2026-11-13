/* -------------------------------------------------------------------------- */
/*                               render/tool.rs                               */
/* -------------------------------------------------------------------------- */

use rig_core::message::ToolResult;
use rig_tools::{EditArgs, WriteArgs};
use yansi::Paint;

use super::palette;

/// Lines a preview keeps of what a call wrote, printed or gave back.
pub(super) const PREVIEW_LINES: usize = 5;

/// Whether calls to `name` show what they change in a file, rather than
/// what they give back.
pub(super) fn shows_changes(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

/// The lines a call to `write` or `edit` adds and removes in its file.
pub(super) fn changes(name: &str, args: &str) -> Option<Vec<(Mark, String)>> {
    let lines = match name {
        "write" => {
            let write = serde_json::from_str::<WriteArgs>(args).ok()?;
            write.content.lines().map(|line| (Mark::Added, line.to_owned())).collect()
        }
        "edit" => {
            let edit = serde_json::from_str::<EditArgs>(args).ok()?;
            edit.edits
                .iter()
                .flat_map(|edit| diff(&edit.old_text, &edit.new_text))
                .map(|(mark, line)| (mark, line.to_owned()))
                .collect()
        }
        _ => return None,
    };
    Some(lines)
}

/// A result's text, whatever else it holds left out.
pub(super) fn text(result: &ToolResult) -> String {
    let text: Vec<_> = result.content.iter().filter_map(|content| content.as_text()).collect();
    text.join("\n")
}

/// What a result's marker says of it: its only line, or how many it has.
pub(super) fn summary(text: &str) -> String {
    match text.lines().count() {
        0 => "done".to_owned(),
        1 => text.trim().to_owned(),
        lines => format!("{lines} lines"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Printed,
    Added,
    Removed,
}

impl Mark {
    /// How a preview's lines mark it.
    pub(super) fn sign(self) -> &'static str {
        match self {
            Mark::Printed => "",
            Mark::Added => "+ ",
            Mark::Removed => "- ",
        }
    }
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

/// A line under a call, shown in full: what it printed or gave back, or
/// what it writes to a file.
pub(super) fn line(mark: Mark, text: &str) -> String {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let muted = palette().muted;
    match mark {
        Mark::Printed => format!("  {} {}", muted.paint("│"), muted.paint(text)),
        Mark::Added => format!("  {}", format!("+ {text}").green()),
        Mark::Removed => format!("  {}", format!("- {text}").red()),
    }
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
