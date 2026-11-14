/* -------------------------------------------------------------------------- */
/*                             compaction/tool.rs                             */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use super::TOOL;
use crate::host::Run;

const GUIDANCE: &str = "Compacts this conversation: before your next step, its older messages \
are replaced by a summary of them, and the newest stay as they are. Call it when much of what the \
conversation holds is no longer needed, such as after finishing one task before starting an \
unrelated one, or after long tool outputs you have taken what you need from. Do not call it in the \
middle of work whose details you still need. A conversation that grows long is compacted on its \
own, so you need not call it to stay within your context window.";

/// The tool that has the run compact before its next model call.
pub struct Compact {
    pub run: Arc<Run>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactArgs {
    /// What the summary should keep above all, such as the task you go on with.
    #[serde(default)]
    pub focus: Option<String>,
}

impl Tool for Compact {
    const NAME: &'static str = TOOL;
    type Args = CompactArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<CompactArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(&self, _context: &mut ToolContext, args: CompactArgs) -> Result<String, ToolExecutionError> {
        match self.run.compact(args.focus.as_deref().unwrap_or_default()) {
            true => Ok("The conversation is compacted before your next step; go on with the work.".into()),
            false => Err(ToolExecutionError::new(
                ToolErrorKind::Other,
                "this answer is ending, so there is no next step to compact before",
            )),
        }
    }
}
