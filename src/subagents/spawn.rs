/* -------------------------------------------------------------------------- */
/*                             subagents/spawn.rs                             */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use futures::future::join_all;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Assignment, Crew, TOOL, Team};
use crate::agent;
use crate::config::Thinking;
use crate::host::Run;
use crate::permissions::Reporter;
use crate::protocol::{AgentId, Ask, ClientInfo, Decision, Event, Task, TaskResult};

/// Most agents one call starts.
const MAX_AGENTS: usize = 8;

/// Based on Zed's guidance for its `spawn_agent` tool.
const GUIDANCE: &str = "Starts subagents: agents that each work on one task in a directory, \
on this machine or another, and reply with a report. Several tasks in one call run at the same time.

- A subagent does not see this conversation. Give each a self-contained message with all the \
context it needs (paths, requirements, constraints) and say what to report.
- Delegate concrete, well-scoped tasks that materially advance the main task, not what one or two \
tool calls would do.
- Put independent tasks in one call so they run in parallel. Give agents that edit files disjoint \
sets of files.
- Do not repeat work you delegated: coordinate, and combine what the agents report.
- To follow up with an agent, pass its `agent` id and only the new message; it remembers its \
earlier work.
- The user may be asked to let an agent into its directory, and may refuse.

You get only each agent's final reply, with its id.";

/// The tool that starts subagents for a run on this host: here, or on the
/// remote client following the run, which takes up the task itself.
pub struct Spawn {
    pub crew: Arc<Crew>,
    pub team: Arc<Team>,
    pub run: Arc<Run>,
    /// The remote client that started the run.
    pub client: Option<ClientInfo>,
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
    /// The id of an agent from an earlier call, to continue its conversation where it
    /// works; leave it out to start a new agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// The machine it works on; default the one you run on.
    #[serde(default)]
    pub machine: Option<String>,
    /// The directory it works in: absolute, `~/...`, or relative to the machine's working
    /// directory; default the working directory.
    #[serde(default)]
    pub directory: Option<String>,
}

/// How one agent's task went, as the model gets it.
#[derive(Debug, Serialize)]
pub struct Report {
    agent: Option<AgentId>,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Report {
    fn new(agent: Option<AgentId>, label: String, result: TaskResult) -> Self {
        let (output, error) = match result {
            Ok(output) => (Some(output), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            agent,
            label,
            output,
            error,
        }
    }
}

impl Tool for Spawn {
    const NAME: &'static str = TOOL;
    type Args = SpawnArgs;
    type Output = Vec<Report>;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        let machines: String = self
            .machines()
            .map(|(name, directory, role)| format!("\n- `{name}`: {role}; working directory {directory}"))
            .collect();
        format!("{GUIDANCE}\n\nMachines:{machines}")
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
    /// Each machine an agent can work on: its name, working directory and
    /// what it is to the model.
    fn machines(&self) -> impl Iterator<Item = (String, String, &'static str)> {
        let host = (
            self.crew.machine().to_owned(),
            agent::working_directory().display().to_string(),
            "where you run",
        );
        let client = self.client.as_ref().and_then(|client| {
            let name = client.subagents.clone().filter(|name| name != self.crew.machine())?;
            let directory = client.machine.cwd.clone().unwrap_or_else(|| "unknown".into());
            Some((name, directory, "the user's"))
        });
        std::iter::once(host).chain(client)
    }

    async fn start(&self, args: TaskArgs) -> Report {
        let (agent, assignment, follow_up) = match self.assign(&args) {
            Ok(assigned) => assigned,
            Err(error) => return Report::new(None, args.label, Err(error)),
        };
        self.run.emit(Event::AgentStarted {
            agent: agent.clone(),
            label: args.label.clone(),
            machine: assignment.machine.clone(),
            directory: assignment.directory.clone(),
        });
        let task = Task {
            agent: agent.clone(),
            label: args.label,
            machine: assignment.machine,
            directory: assignment.directory,
            message: args.message,
            follow_up,
            model: self.model.clone(),
            thinking: self.thinking,
        };
        let result = match task.machine == self.crew.machine() {
            true => {
                let reporter = Tagged {
                    run: self.run.clone(),
                    agent: agent.clone(),
                    label: task.label.clone(),
                };
                self.crew.run(&self.team, &task, reporter).await
            }
            false => self.run.delegate(task.clone()).await,
        };
        self.run.emit(Event::AgentEnded {
            agent: agent.clone(),
            error: result.as_ref().err().cloned(),
        });
        Report::new(Some(agent), task.label, result)
    }

    /// The agent `args` continues, else a new one where they say; and
    /// whether it continues one.
    fn assign(&self, args: &TaskArgs) -> Result<(AgentId, Assignment, bool), String> {
        if let Some(agent) = &args.agent {
            let agent = AgentId(agent.clone());
            let assignment = self
                .team
                .assignment(&agent)
                .ok_or_else(|| format!("no agent `{agent}` in this conversation; leave `agent` out to start one"))?;
            return Ok((agent, assignment, true));
        }
        let machine = args
            .machine
            .clone()
            .unwrap_or_else(|| self.crew.machine().to_owned());
        if !self.machines().any(|(name, ..)| name == machine) {
            let names: Vec<String> = self.machines().map(|(name, ..)| format!("`{name}`")).collect();
            return Err(format!("no machine `{machine}`; use {}", names.join(" or ")));
        }
        let assignment = Assignment {
            machine,
            directory: args.directory.clone(),
        };
        let agent = AgentId::new();
        self.team.assign(agent.clone(), assignment.clone());
        Ok((agent, assignment, false))
    }
}

/// Shows a subagent on this host as part of its parent's run.
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
        self.run.ask(labelled(ask, &self.label)).await
    }

    async fn stop(&self) {
        self.run.stop();
    }
}

/// `ask`, saying which subagent's tool call it is about.
pub fn labelled(ask: Ask, label: &str) -> Ask {
    match ask {
        Ask::Tool { name, .. } => Ask::Tool {
            name,
            agent: Some(label.to_owned()),
        },
        directory => directory,
    }
}
