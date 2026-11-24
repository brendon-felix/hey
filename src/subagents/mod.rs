/* -------------------------------------------------------------------------- */
/*                              subagents/mod.rs                              */
/* -------------------------------------------------------------------------- */

//! Subagents: agents the main agent starts on the host to work on one task
//! each. They have its tools, so they work on the host or on the client
//! that lends its own, as their calls say.

mod kinds;
mod spawn;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use rig_agent::agent::Agent;
use rig_core::completion::GenerationOptions;
use rig_memory::MemoryStore;
use rig_tools::Seen;

use crate::agent::{self, Model};
use crate::config::{Config, Providers, Thinking};
use crate::gate;
use crate::hooks::{Scope, ToolHooks};
use crate::permissions::{Permissions, Reporter};
use crate::protocol::{AgentId, AgentInfo, AgentSnapshot, Event, Working};
use crate::results::ResultLimit;
use crate::route::{Kit, Tag};
use crate::sessions::SessionId;
use crate::skills;

pub use kinds::{Kind, Kinds, find as kinds};
pub use spawn::Spawn;

/// The name the model calls the tool that starts subagents by.
pub const TOOL: &str = "spawn_agents";

const PROMPT: &str = "You are a subagent of hey, working on one task that the main agent \
delegated to you. You cannot see its conversation or ask the user anything: the task is all you \
know. Use your tools rather than guessing. When you are done, reply with a short report of what \
you found or changed, naming files by path and the machine they are on: that reply is all the \
main agent sees.";

/// The parts of the main agent's guidance that a subagent needs as much.
const GUIDANCE: &str = "\nGuidance:\n\
- Change files only with the file tools, never through the shell: `edit` (its `edits` array takes \
many changes in one call), `write`, `move` and `delete`. Never use `sed`, `awk`, `perl`, `python`, \
`tee`, `echo >`, `cat >`, heredocs or any other command to write or patch a file, even for a big \
change. Before each shell call, ask whether it changes a file; if so, use a file tool. Likewise \
`read`, `search` and `list` rather than `cat`, `grep`, `ls` or `find`. Keep the shell for what \
the tools cannot do, such as builds, tests and git.\n\
- Tool arguments are one JSON object each. `edit` takes `path` and either `old_text` and \
`new_text` for one change, or `edits`, a real JSON array of such pairs, not a string of JSON.\n\
- When calls do not depend on one another, make them in the same step; calls that only read run \
together, others one after another.\n\
- Never invent paths, output or results; learn a file's real path before editing it, and read \
only the parts of large files that matter. Bound commands that may run long by a timeout or the \
background.\n\
- Never claim something passed unless you ran it and saw it pass. Do not commit, push, delete or \
revert work you did not make, and do not hardcode or expose secrets.\n\
- If a tool call is refused or cannot run, say so in your report instead of retrying it or \
working around the refusal.\n\
- Tools with a `machine` argument run on the machine it names; files are on that machine only.";

/// What the host needs to run subagents: the model settings its config
/// gives them, and what each conversation's subagents have done.
pub struct Crew {
    providers: Providers,
    max_turns: usize,
    max_tokens: Option<u64>,
    result_bytes: usize,
    teams: Mutex<HashMap<SessionId, Arc<Team>>>,
}

/// A conversation's subagents.
pub struct Team {
    /// The conversation's own, so "always allow" holds for all its agents.
    permissions: Arc<Permissions>,
    /// Their conversations, for follow-ups.
    store: MemoryStore,
    seen: Mutex<HashMap<AgentId, Seen>>,
    /// The kind each agent started as, which its follow-ups keep.
    kinds: Mutex<HashMap<AgentId, Kind>>,
    /// Every agent, in the order they first started, for clients to view.
    roster: Mutex<Vec<Member>>,
}

struct Member {
    agent: AgentId,
    /// The label of its latest task.
    label: String,
    task: Option<Task>,
}

/// A task a subagent works on, until its conversation saves it.
struct Task {
    message: String,
    started: Instant,
    steps: Vec<Event>,
}

/// One turn of a subagent's conversation.
pub struct Turn<'a> {
    pub agent: &'a AgentId,
    pub label: &'a str,
    pub message: &'a str,
    pub model: &'a str,
    pub thinking: Option<Thinking>,
    /// The kind of agent, whose prompt, tools, model and thinking it has.
    pub kind: Option<&'a Kind>,
    /// The conversation's hooks, which the subagent's tool calls run too.
    pub hooks: Option<&'a Scope>,
    /// The project's `AGENTS.md` instructions, for the end of the prompt.
    pub instructions: Option<&'a str>,
}

impl Crew {
    pub fn new(config: &Config) -> Self {
        Self {
            providers: config.providers.clone(),
            max_turns: config.max_turns,
            max_tokens: config.max_tokens,
            result_bytes: config.tools.result_bytes,
            teams: Mutex::default(),
        }
    }

    /// The subagents of conversation `thread`, sharing its `permissions`.
    pub fn team(&self, thread: &SessionId, permissions: Arc<Permissions>) -> Arc<Team> {
        let mut teams = lock(&self.teams);
        let team = teams
            .entry(thread.clone())
            .or_insert_with(|| Arc::new(Team::new(permissions)));
        team.clone()
    }

    pub fn forget(&self, thread: &SessionId) {
        lock(&self.teams).remove(thread);
    }

    /// The subagents of conversation `thread`, if it has had any since the
    /// host started.
    pub fn find(&self, thread: &SessionId) -> Option<Arc<Team>> {
        lock(&self.teams).get(thread).cloned()
    }

    /// Runs `turn` with the tools of `kit`, reporting its tool calls and
    /// asking its questions through `reporter`. Gives the agent's final
    /// reply.
    pub async fn run(
        &self,
        team: &Team,
        kit: &Kit,
        turn: Turn<'_>,
        reporter: impl Reporter,
    ) -> Result<String, String> {
        let model_name = turn
            .kind
            .and_then(|kind| kind.model.as_deref())
            .unwrap_or(turn.model);
        let model =
            Model::connect(model_name, &self.providers).map_err(|error| format!("{error:#}"))?;
        let agent = self.agent(team, kit, &turn, &model, reporter);
        let runner = agent
            .prompt(turn.message.to_owned())
            .conversation(turn.agent.as_str())
            .tool_concurrency(gate::CONCURRENCY);
        let runner = match turn.kind.and_then(|kind| kind.thinking).or(turn.thinking) {
            Some(thinking) => runner.options(GenerationOptions::default().reasoning(thinking)),
            None => runner,
        };
        runner
            .await
            .map(|response| response.output())
            .map_err(|error| error.to_string())
    }

    fn agent(
        &self,
        team: &Team,
        kit: &Kit,
        turn: &Turn<'_>,
        model: &Model,
        reporter: impl Reporter,
    ) -> Agent {
        let tag = Tag {
            agent: turn.agent.clone(),
            label: turn.label.to_owned(),
        };
        let (mut tools, routes) = kit.tools(&team.seen(turn.agent), None, Some(tag));
        if let Some(kind) = turn.kind {
            tools.retain(|tool| kind.allows(&tool.name()));
        }
        let hooks = turn
            .hooks
            .map(|scope| ToolHooks::new(scope.clone(), kit.run.clone()));
        let cwd = agent::working_directory();
        let client = kit.lent.as_ref().map(|lent| lent.machine.as_str());
        let prompt = turn.kind.map_or(PROMPT, |kind| kind.prompt.as_str());
        let builder = agent::builder(
            model,
            tools,
            routes,
            team.permissions.clone(),
            Arc::new(reporter),
            hooks,
        )
        .name("hey subagent")
        .preamble(prompt)
        .append_preamble(&environment(&cwd, &kit.host, client))
        .append_preamble(GUIDANCE);
        let builder = match agent::spill_dir() {
            Some(dir) => builder.append_preamble(&format!(
                "\nTool output too long to give you whole is kept whole in {}.",
                dir.display()
            )),
            None => builder,
        };
        let builder = match turn.instructions {
            Some(instructions) => builder.append_preamble(instructions),
            None => builder,
        };
        let builder = match kit.toolbox.preamble() {
            Some(deferred) => builder.append_preamble(&deferred),
            None => builder,
        };
        let host_skills = skills::find(&cwd);
        let builder = match turn.kind.is_none_or(|kind| kind.allows(skills::TOOL)) {
            true => host_skills.equip(builder),
            false => builder,
        };
        let builder = match kit.lent.as_ref().and_then(|lent| {
            lent.deferred
                .as_deref()
                .map(|listed| crate::mcp::deferred::client_preamble(&lent.machine, listed))
        }) {
            Some(deferred) => builder.append_preamble(&deferred),
            None => builder,
        };
        let on_client = kit
            .lent
            .as_ref()
            .filter(|_| turn.kind.is_none_or(|kind| kind.allows("read")));
        let builder = match on_client.and_then(|lent| {
            skills::client_catalog(&lent.machine, &kit.client_skills, &host_skills.skills)
        }) {
            Some(catalog) => builder.append_preamble(&catalog),
            None => builder,
        };
        let mut builder = builder
            .default_max_turns(self.max_turns)
            .add_hook(crate::budget::Budget::new(self.max_turns, None))
            .add_hook(crate::recovery::Recovery)
            .memory(team.store.clone())
            .add_hook(ResultLimit {
                bytes: self.result_bytes,
                spill: agent::spill(),
            });
        if let Some(max_tokens) = self.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }
}

impl Team {
    fn new(permissions: Arc<Permissions>) -> Self {
        Self {
            permissions,
            store: MemoryStore::default(),
            seen: Mutex::default(),
            kinds: Mutex::default(),
            roster: Mutex::default(),
        }
    }

    /// The kind `agent` started as, if any.
    fn kind(&self, agent: &AgentId) -> Option<Kind> {
        lock(&self.kinds).get(agent).cloned()
    }

    fn set_kind(&self, agent: &AgentId, kind: Kind) {
        lock(&self.kinds).insert(agent.clone(), kind);
    }

    /// Whether `agent` said anything in this conversation before.
    pub fn remembers(&self, agent: &AgentId) -> bool {
        rig_memory::load(&self.store, agent.as_str())
            .is_ok_and(|conversation| !conversation.messages().is_empty())
    }

    fn seen(&self, agent: &AgentId) -> Seen {
        lock(&self.seen).entry(agent.clone()).or_default().clone()
    }

    /// `agent` starts work on `message`, under `label`.
    fn begin(&self, agent: &AgentId, label: &str, message: &str) {
        let task = Task {
            message: message.to_owned(),
            started: Instant::now(),
            steps: Vec::new(),
        };
        let mut roster = lock(&self.roster);
        match roster.iter_mut().find(|member| &member.agent == agent) {
            Some(member) => {
                member.label = label.to_owned();
                member.task = Some(task);
            }
            None => roster.push(Member {
                agent: agent.clone(),
                label: label.to_owned(),
                task: Some(task),
            }),
        }
    }

    fn step(&self, agent: &AgentId, event: &Event) {
        let mut roster = lock(&self.roster);
        let task = roster
            .iter_mut()
            .find(|member| &member.agent == agent)
            .and_then(|member| member.task.as_mut());
        if let Some(task) = task {
            task.steps.push(event.clone());
        }
    }

    /// `agent` is done with its task, which its conversation holds if it
    /// went well.
    fn end(&self, agent: &AgentId) {
        if let Some(member) = lock(&self.roster)
            .iter_mut()
            .find(|member| &member.agent == agent)
        {
            member.task = None;
        }
    }

    pub fn roster(&self) -> Vec<AgentInfo> {
        lock(&self.roster)
            .iter()
            .map(|member| AgentInfo {
                agent: member.agent.clone(),
                label: member.label.clone(),
                working: member.task.is_some(),
            })
            .collect()
    }

    pub fn snapshot(&self, agent: &AgentId) -> Option<AgentSnapshot> {
        let (label, working) = {
            let roster = lock(&self.roster);
            let member = roster.iter().find(|member| &member.agent == agent)?;
            let working = member.task.as_ref().map(|task| Working {
                message: task.message.clone(),
                elapsed: task.started.elapsed(),
                steps: task.steps.clone(),
            });
            (member.label.clone(), working)
        };
        let messages = rig_memory::load(&self.store, agent.as_str())
            .map(|conversation| {
                conversation
                    .messages()
                    .iter()
                    .map(|message| rig_core::message::Message::clone(message))
                    .collect()
            })
            .unwrap_or_default();
        Some(AgentSnapshot {
            label,
            messages,
            working,
        })
    }
}

/// Where a subagent works, for the end of its system prompt.
fn environment(dir: &Path, host: &str, client: Option<&str>) -> String {
    let client = client
        .map(|client| {
            format!(
                "\n- The user's machine: `{client}`; tools with a `machine` argument can run there"
            )
        })
        .unwrap_or_default();
    let windows = match std::env::consts::OS {
        "windows" => {
            "\n- This machine runs Windows: in `shell` calls use only shells and commands Windows \
             supports (PowerShell, cmd or the named shell), not `sh`, bash, `&&`, `$(...)`, \
             `/dev/null` or Unix tools; prefer the dedicated file tools"
        }
        _ => "",
    };
    format!(
        "\nEnvironment:\n- Machine: `{host}` ({}, {}), where your tools run\n- Working directory: \
         {}{client}{windows}\n- Date: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        dir.display(),
        chrono::Local::now().format("%Y-%m-%d"),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
