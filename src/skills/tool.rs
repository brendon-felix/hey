/* -------------------------------------------------------------------------- */
/*                               skills/tool.rs                               */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Skill, TOOL};

/// The tool that loads a skill's instructions: only of the skills the
/// model may load, so it cannot learn even the names of the others.
pub struct Load {
    pub skills: Arc<[Skill]>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadArgs {
    /// The skill's name, as listed in `<available_skills>`.
    pub name: String,
}

impl Tool for Load {
    const NAME: &'static str = TOOL;
    type Args = LoadArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Loads the instructions of a skill listed in `<available_skills>`. Call it when a task \
         matches a skill's description, before starting on the task, then follow what it returns."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<LoadArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(&self, _context: &mut ToolContext, args: LoadArgs) -> Result<String, ToolExecutionError> {
        let Some(skill) = self.skills.iter().find(|skill| skill.name.as_str() == args.name) else {
            let names: Vec<&str> = self.skills.iter().map(|skill| skill.name.as_str()).collect();
            return Err(ToolExecutionError::new(
                ToolErrorKind::NotFound,
                format!("no skill `{}`; the skills are {}", args.name, names.join(", ")),
            ));
        };
        skill
            .content()
            .map_err(|error| ToolExecutionError::new(ToolErrorKind::Other, format!("{error:#}")))
    }
}
