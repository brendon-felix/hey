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

use rig_agent::agent::Agent;
use rig_core::completion::GenerationOptions;
use rig_memory::MemoryStore;
use rig_tools::Seen;

use crate::agent::{self, Model};
use crate::config::{Config, Providers, Thinking};
use crate::gate;
use crate::hooks::{Scope, ToolHooks};
use crate::permissions::{Permissions, Reporter};
use crate::protocol::AgentId;
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

    /// Runs `turn` with the tools of `kit`, reporting its tool calls and
    /// asking its questions through `reporter`. Gives the agent's final
    /// reply.
    pub async fn run(&self, team: &Team, kit: &Kit, turn: Turn<'_>, reporter: impl Reporter) -> Result<String, String> {
        let model_name = turn.kind.and_then(|kind| kind.model.as_deref()).unwrap_or(turn.model);
        let model = Model::connect(model_name, &self.providers).map_err(|error| format!("{error:#}"))?;
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

    fn agent(&self, team: &Team, kit: &Kit, turn: &Turn<'_>, model: &Model, reporter: impl Reporter) -> Agent {
        let tag = Tag {
            agent: turn.agent.clone(),
            label: turn.label.to_owned(),
        };
        let (mut tools, routes) = kit.tools(&team.seen(turn.agent), None, Some(tag));
        if let Some(kind) = turn.kind {
            tools.retain(|tool| kind.allows(&tool.name()));
        }
        let hooks = turn.hooks.map(|scope| ToolHooks::new(scope.clone(), kit.run.clone()));
        let cwd = agent::working_directory();
        let client = kit.lent.as_ref().map(|lent| lent.machine.as_str());
        let prompt = turn.kind.map_or(PROMPT, |kind| kind.prompt.as_str());
        let builder = agent::builder(model, tools, routes, team.permissions.clone(), Arc::new(reporter), hooks)
            .name("hey subagent")
            .preamble(prompt)
            .append_preamble(&environment(&cwd, &kit.host, client));
        let builder = match kit.toolbox.preamble() {
            Some(deferred) => builder.append_preamble(&deferred),
            None => builder,
        };
        let builder = match turn.kind.is_none_or(|kind| kind.allows(skills::TOOL)) {
            true => skills::find(&cwd).equip(builder),
            false => builder,
        };
        let mut builder = builder
            .default_max_turns(self.max_turns)
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
    format!(
        "\nEnvironment:\n- Machine: `{host}` ({}, {}), where your tools run\n- Working directory: \
         {}{client}\n- Date: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        dir.display(),
        chrono::Local::now().format("%Y-%m-%d"),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
