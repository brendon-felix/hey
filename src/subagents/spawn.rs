/* -------------------------------------------------------------------------- */
/*                             subagents/spawn.rs                             */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use futures::future::join_all;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Crew, TOOL, Team, Turn};
use crate::config::Thinking;
use crate::host::Run;
use crate::permissions::Reporter;
use crate::protocol::{AgentId, Ask, Decision, Event};
use crate::route::Kit;

/// Most agents one call starts.
const MAX_AGENTS: usize = 8;

/// Based on Zed's guidance for its `spawn_agent` tool.
const GUIDANCE: &str = "Starts subagents: agents that each work on one task with the tools you \
have, on the same machines, and reply with a report. Several tasks in one call run at the same \
time.

- A subagent does not see this conversation. Give each a self-contained message with all the \
context it needs (paths, the machine they are on, requirements, constraints) and say what to \
report.
- Delegate concrete, well-scoped tasks that materially advance the main task, not what one or two \
tool calls would do.
- Put independent tasks in one call so they run in parallel. Give agents that edit files disjoint \
sets of files.
- Do not repeat work you delegated: coordinate, and combine what the agents report.
- To follow up with an agent, pass its `agent` id and only the new message; it remembers its \
earlier work.

You get only each agent's final reply, with its id.";

/// The tool that starts subagents for a run, on the host, with the run's
/// tools.
pub struct Spawn {
    pub crew: Arc<Crew>,
    pub team: Arc<Team>,
    pub kit: Kit,
    pub model: String,
    pub thinking: Option<Thinking>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnArgs {
    /// The tasks, run at the same time.
    pub agents: Vec<TaskArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    /// A few words shown to the user while it works, e.g. "Finding the config loader".
    pub label: String,
    /// What to do. A new agent knows nothing else, so give it all the context it needs; a
    /// follow-up needs only the new message.
    pub message: String,
    /// The id of an agent from an earlier call, to continue its conversation; leave it out
    /// to start a new agent.
    #[serde(default)]
    pub agent: Option<String>,
}

/// How one agent's task went, as the model gets it.
#[derive(Debug, Serialize)]
pub struct Report {
    agent: AgentId,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Tool for Spawn {
    const NAME: &'static str = TOOL;
    type Args = SpawnArgs;
    type Output = Vec<Report>;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<SpawnArgs>()
    }

    async fn call(&self, _context: &mut ToolContext, args: SpawnArgs) -> Result<Vec<Report>, ToolExecutionError> {
        if args.agents.len() > MAX_AGENTS {
            return Err(ToolExecutionError::new(
                ToolErrorKind::InvalidArgs,
                format!("start at most {MAX_AGENTS} agents at once"),
            ));
        }
        Ok(join_all(args.agents.into_iter().map(|task| self.start(task))).await)
    }
}

impl Spawn {
    async fn start(&self, args: TaskArgs) -> Report {
        let (agent, result) = match args.agent.map(AgentId) {
            Some(agent) if !self.team.remembers(&agent) => {
                let error = format!("no agent `{agent}` in this conversation; leave `agent` out to start one");
                (agent, Err(error))
            }
            named => {
                let agent = named.unwrap_or_else(AgentId::new);
                let result = self.work(&agent, &args.label, &args.message).await;
                (agent, result)
            }
        };
        let (output, error) = match result {
            Ok(output) => (Some(output), None),
            Err(error) => (None, Some(error)),
        };
        Report {
            agent,
            label: args.label,
            output,
            error,
        }
    }

    async fn work(&self, agent: &AgentId, label: &str, message: &str) -> Result<String, String> {
        let run = &self.kit.run;
        run.emit(Event::AgentStarted {
            agent: agent.clone(),
            label: label.to_owned(),
        });
        let turn = Turn {
            agent,
            label,
            message,
            model: &self.model,
            thinking: self.thinking,
        };
        let reporter = Tagged {
            run: run.clone(),
            agent: agent.clone(),
            label: label.to_owned(),
        };
        let result = self.crew.run(&self.team, &self.kit, turn, reporter).await;
        run.emit(Event::AgentEnded {
            agent: agent.clone(),
            error: result.as_ref().err().cloned(),
        });
        result
    }
}

/// Shows a subagent as part of its parent's run.
struct Tagged {
    run: Arc<Run>,
    agent: AgentId,
    label: String,
}

impl Reporter for Tagged {
    async fn emit(&self, event: Event) {
        self.run.emit(Event::Agent {
            agent: self.agent.clone(),
            step: Box::new(event),
        });
    }

    async fn ask(&self, ask: Ask) -> Decision {
        let ask = match ask {
            Ask::Tool { name, machine, .. } => Ask::Tool {
                name,
                agent: Some(self.label.clone()),
                machine,
            },
            directory => directory,
        };
        self.run.ask(ask).await
    }

    async fn stop(&self) {
        self.run.stop();
    }
}
