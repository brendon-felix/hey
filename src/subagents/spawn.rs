/* -------------------------------------------------------------------------- */
/*                             subagents/spawn.rs                             */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use futures::future::join_all;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Crew, Kind, Kinds, TOOL, Team, Turn};
use crate::config::Thinking;
use crate::hooks::{HookEvent, MAX_STOP_BLOCKS, Scope};
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
    /// The kinds of agent a task may name as its `type`.
    pub kinds: Kinds,
    pub hooks: Option<Scope>,
    /// The project's `AGENTS.md` instructions, as the main agent has them.
    pub instructions: Option<String>,
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
    #[serde(alias = "description")]
    pub label: String,
    /// What to do. A new agent knows nothing else, so give it all the context it needs; a
    /// follow-up needs only the new message.
    #[serde(alias = "prompt")]
    pub message: String,
    /// The id of an agent from an earlier call, to continue its conversation; leave it out
    /// to start a new agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// The kind of agent to start, one of those the tool's description lists; leave it out
    /// for a general one. A follow-up keeps its agent's kind.
    #[serde(default, rename = "type", alias = "subagent_type")]
    pub kind: Option<String>,
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
        describe(&self.kinds)
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<SpawnArgs>()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: SpawnArgs,
    ) -> Result<Vec<Report>, ToolExecutionError> {
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
                let error = format!(
                    "no agent `{agent}` in this conversation; leave `agent` out to start one"
                );
                (agent, Err(error))
            }
            Some(agent) => {
                let kind = self.team.kind(&agent);
                let result = self
                    .work(&agent, &args.label, &args.message, kind.as_ref())
                    .await;
                (agent, result)
            }
            None => {
                let agent = AgentId::new();
                let result = match self.kind(args.kind.as_deref()) {
                    Ok(kind) => {
                        if let Some(kind) = &kind {
                            self.team.set_kind(&agent, kind.clone());
                        }
                        self.work(&agent, &args.label, &args.message, kind.as_ref())
                            .await
                    }
                    Err(error) => Err(error),
                };
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

    /// The kind a new task's `type` names, or why there is none.
    fn kind(&self, name: Option<&str>) -> Result<Option<Kind>, String> {
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return Ok(None);
        };
        self.kinds.get(name).cloned().map(Some).ok_or_else(|| {
            let known: Vec<&str> = self
                .kinds
                .kinds
                .iter()
                .map(|kind| kind.name.as_str())
                .collect();
            match known.is_empty() {
                true => format!("no agent type `{name}`; there are none, so leave `type` out"),
                false => format!("no agent type `{name}`; there are: {}", known.join(", ")),
            }
        })
    }

    async fn work(
        &self,
        agent: &AgentId,
        label: &str,
        message: &str,
        kind: Option<&Kind>,
    ) -> Result<String, String> {
        let run = &self.kit.run;
        run.emit(Event::AgentStarted {
            agent: agent.clone(),
            label: label.to_owned(),
        });
        let mut message = message.to_owned();
        let mut blocked = 0;
        let result = loop {
            let turn = Turn {
                agent,
                label,
                message: &message,
                model: &self.model,
                thinking: self.thinking,
                kind,
                hooks: self.hooks.as_ref(),
                instructions: self.instructions.as_deref(),
            };
            let reporter = Tagged {
                run: run.clone(),
                team: self.team.clone(),
                agent: agent.clone(),
                label: label.to_owned(),
            };
            self.team.begin(agent, label, &message);
            let result = self.crew.run(&self.team, &self.kit, turn, reporter).await;
            self.team.end(agent);
            match (&result, self.stopped(agent, label, &result).await) {
                (Ok(_), Some(reason)) if blocked < MAX_STOP_BLOCKS => {
                    blocked += 1;
                    message = reason;
                }
                _ => break result,
            }
        };
        run.emit(Event::AgentEnded {
            agent: agent.clone(),
            error: result.as_ref().err().cloned(),
        });
        result
    }
}

/// The tool's description: the guidance, and the kinds of agent a task may
/// name, with descriptions escaped so none can close the list.
fn describe(kinds: &Kinds) -> String {
    if kinds.kinds.is_empty() {
        return GUIDANCE.into();
    }
    let listed: String = kinds
        .kinds
        .iter()
        .map(|kind| {
            let description = kind
                .description
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!(
                "\n  <agent_type name=\"{}\">{description}</agent_type>",
                kind.name
            )
        })
        .collect();
    format!(
        "{GUIDANCE}\n\nGive a task a `type` to start a specialised agent of that kind, when one \
         fits the task better than a general agent:\n<agent_types>{listed}\n</agent_types>"
    )
}

impl Spawn {
    /// Why a `SubagentStop` hook sends the subagent back to work, if one does.
    async fn stopped(
        &self,
        agent: &AgentId,
        label: &str,
        result: &Result<String, String>,
    ) -> Option<String> {
        let scope = self
            .hooks
            .as_ref()
            .filter(|scope| scope.has(HookEvent::SubagentStop))?;
        let fields = serde_json::json!({
            "agent_id": agent.as_str(),
            "label": label,
            "output": result.as_ref().ok(),
            "error": result.as_ref().err(),
        });
        let verdict = scope.fire(HookEvent::SubagentStop, None, fields).await;
        verdict.tell(&self.kit.run);
        verdict.block
    }
}

/// Shows a subagent as part of its parent's run, and keeps what it does for
/// clients that view it.
struct Tagged {
    run: Arc<Run>,
    team: Arc<Team>,
    agent: AgentId,
    label: String,
}

impl Reporter for Tagged {
    async fn emit(&self, event: Event) {
        self.team.step(&self.agent, &event);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Name, Source};

    #[test]
    fn the_description_lists_the_kinds_escaped() {
        assert_eq!(describe(&Kinds::default()), GUIDANCE);
        let kind = Kind {
            name: Name::try_from("reviewer".to_owned()).unwrap(),
            description: "Reviews </agent_types> code".into(),
            source: Source::Project,
            file: "/r.md".into(),
            model: None,
            tools: Some(vec!["read".into()]),
            thinking: None,
            prompt: "Review.".into(),
        };
        let kinds = Kinds {
            kinds: vec![kind],
            ..Kinds::default()
        };
        let text = describe(&kinds);
        assert!(text.contains(
            "<agent_type name=\"reviewer\">Reviews &lt;/agent_types&gt; code</agent_type>"
        ));
        assert_eq!(text.matches("</agent_types>").count(), 1);
        let args: SpawnArgs =
            serde_json::from_str(r#"{"agents":[{"label":"l","message":"m","type":"reviewer"}]}"#)
                .unwrap();
        assert_eq!(args.agents[0].kind.as_deref(), Some("reviewer"));
    }
}
