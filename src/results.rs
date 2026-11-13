/* -------------------------------------------------------------------------- */
/*                                 results.rs                                 */
/* -------------------------------------------------------------------------- */

use std::io::{self, Write};
use std::path::PathBuf;

use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_core::message::ToolResultContent;
use rig_core::tool::ToolOutput;
use rig_tools::Spill;

use crate::{mcp, web};

/// Cuts the result of a tool whose results know no bound, an MCP server's
/// or one reading the web, to at most `bytes` before the model reads it,
/// keeping the whole in a file it can `read` or `search`. The model reads
/// every byte of a result before it can answer, at a few seconds a page on a
/// local model; the other built-in tools bound their own.
pub struct ResultLimit {
    pub bytes: usize,
    pub spill: Option<Spill>,
}

impl AgentHook for ResultLimit {
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        let (Some(tool), Some(result)) = (event.tool_name(), event.tool_result()) else {
            return OutcomeAction::proceed();
        };
        match unbounded(tool).then(|| self.cut(tool, result.output())) {
            Some(Some(output)) => OutcomeAction::rewrite_tool_output(&event, output),
            _ => OutcomeAction::proceed(),
        }
    }
}

impl ResultLimit {
    /// `output` cut to size, or `None` when it fits.
    fn cut(&self, tool: &str, output: &ToolOutput) -> Option<ToolOutput> {
        let blocks = output.as_content();
        if size(blocks) <= self.bytes {
            return None;
        }
        let whole = text(blocks);
        let kept = head(&whole, self.bytes);
        let read_on = match kept.strip_suffix('\n') {
            Some(lines) => format!("from offset {} for the rest", lines.lines().count() + 1),
            None => format!(
                "with offset {} and limit 1 for the rest of the line cut short",
                kept.lines().count()
            ),
        };
        let whereabouts = match self.keep(tool, &whole) {
            Ok(path) => format!(
                "all of it is in {}: `read` it there {read_on}, or `search` it",
                path.display()
            ),
            Err(error) => format!("the rest could not be kept: {error}"),
        };
        let note = format!(
            "\n[Cut to {} of {} bytes; {whereabouts}]",
            kept.len(),
            whole.len()
        );
        let images = blocks
            .iter()
            .filter(|block| matches!(block, ToolResultContent::Image(_)))
            .cloned();
        let content = std::iter::once(ToolResultContent::text(format!("{kept}{note}")))
            .chain(images)
            .collect();
        ToolOutput::content(content).ok()
    }

    fn keep(&self, tool: &str, whole: &str) -> io::Result<PathBuf> {
        let spill = self
            .spill
            .as_ref()
            .ok_or_else(|| io::Error::other("no cache directory"))?;
        let (path, mut file) = spill.create(tool)?;
        file.write_all(whole.as_bytes())?;
        Ok(path)
    }
}

fn unbounded(tool: &str) -> bool {
    tool.starts_with(mcp::PREFIX) || web::TOOLS.contains(&tool)
}

/// Bytes of text the model reads of `blocks`, JSON as the provider sends it.
fn size(blocks: &[ToolResultContent]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => text.text.len(),
            ToolResultContent::Json { value } => value.to_string().len(),
            ToolResultContent::Image(_) => 0,
        })
        .sum()
}

/// The text of `blocks`, JSON laid out a value a line, so a cut and the
/// kept whole can be read by line.
fn text(blocks: &[ToolResultContent]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.clone()),
            ToolResultContent::Json { value } => {
                Some(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
            }
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// At most `bytes` of `text` from its start: whole lines, unless that would
/// keep less than half, as with one long line.
fn head(text: &str, bytes: usize) -> &str {
    let end = text.floor_char_boundary(bytes);
    let cut = &text[..end];
    match cut.rfind('\n') {
        Some(line_end) if line_end >= end / 2 => &text[..=line_end],
        _ => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_whole_lines_and_says_where_to_read_on() {
        let dir = std::env::temp_dir().join(format!("hey-results-{}", std::process::id()));
        let limit = ResultLimit {
            bytes: 40,
            spill: Some(Spill(dir.clone())),
        };
        let small = ToolOutput::json(serde_json::json!({ "a": 1 }));
        assert!(limit.cut("mcp__x__y", &small).is_none());

        let items: Vec<String> = (0..20).map(|n| format!("item {n}")).collect();
        let big = ToolOutput::json(serde_json::json!({ "items": items }));
        let cut = limit.cut("mcp__x__y", &big).unwrap();
        let text = cut.as_text().unwrap();
        assert!(text.starts_with("{\n  \"items\": [\n    \"item 0\",\n"), "{text}");
        assert!(text.contains("Cut to 29 of"), "{text}");
        assert!(text.contains("from offset 4 for the rest"), "{text}");
        let kept = std::fs::read_to_string(dir.join("mcp__x__y-1.txt")).unwrap();
        assert!(kept.contains("\"item 19\""));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cuts_one_long_line_within_it() {
        assert_eq!(head("ab\nloooooooong", 10), "ab\nloooooo");
        assert_eq!(head("abcdefgh\nij", 10), "abcdefgh\n");
        assert_eq!(head("é", 1), "");
    }
}
