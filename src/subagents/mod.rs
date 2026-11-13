/* -------------------------------------------------------------------------- */
/*                              subagents/mod.rs                              */
/* -------------------------------------------------------------------------- */

//! Subagents: agents the main agent starts to work on one task each, in a
//! directory of the machine it runs on or of the remote client's. Each
//! machine runs its own, under its own config: which directories they may
//! work in, and how their tools are permitted.

mod access;
mod delegated;
mod fence;
mod spawn;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use rig_agent::agent::Agent;
use rig_core::completion::GenerationOptions;
use rig_memory::MemoryStore;
use rig_tools::Seen;

use crate::agent::{self, BUILTIN_TOOLS, Model, Place};
use crate::config::{Config, Providers};
use crate::permissions::{Approval, Origin, Permissions, Reporter};
use crate::protocol::{AgentId, Ask, Decision, Event, Task, TaskResult};
use crate::sessions::SessionId;

pub use access::machine_name;
use access::{Access, Verdict};
pub use delegated::take_up;
use fence::Fenced;
pub use spawn::{Spawn, labelled};

/// The name the model calls the tool that starts subagents by.
pub const TOOL: &str = "spawn_agents";

const PROMPT: &str = "You are a subagent of hey, working on one task that the main agent \
delegated to you. You cannot see its conversation or ask the user anything: the task is all you \
know. Work within your working directory; your tools refuse files outside it and any the user's \
configuration excludes. Use your tools rather than guessing. When you are done, reply with a \
short report of what you found or changed, naming files by path: that reply is all the main \
agent sees.";

/// What this machine needs to run subagents: where they may work, the
/// model settings and tool permissions its config gives them, and what
/// each conversation's subagents have done here.
pub struct Crew {
    access: Access,
    providers: Providers,
    max_turns: usize,
    max_tokens: Option<u64>,
    /// The permissions of a conversation that brings none of its own.
    permissions: Permissions,
    teams: Mutex<HashMap<SessionId, Arc<Team>>>,
}

/// A conversation's subagents on one machine.
pub struct Team {
    permissions: Arc<Permissions>,
    /// Directories the user let them into for this conversation.
    granted: Mutex<Vec<PathBuf>>,
    /// Held while a directory is admitted, so agents that want the same
    /// one at once ask about it once.
    admitting: tokio::sync::Mutex<()>,
    /// Their conversations, for follow-ups.
    store: MemoryStore,
    seen: Mutex<HashMap<AgentId, Seen>>,
    /// The agents this conversation started, wherever they run.
    assignments: Mutex<HashMap<AgentId, Assignment>>,
}

/// Where an agent works, for every turn of its conversation.
#[derive(Debug, Clone)]
pub struct Assignment {
    pub machine: String,
    pub directory: Option<String>,
}

impl Crew {
    pub fn new(config: &Config) -> Result<Self> {
        let machine = machine_name();
        let directories = config.subagents.machines.get(&machine);
        let access = Access::new(machine, directories, config.file.clone())?;
        let builtin = BUILTIN_TOOLS.map(|name| (name, &Origin::Builtin));
        Ok(Self {
            access,
            providers: config.providers.clone(),
            max_turns: config.max_turns,
            max_tokens: config.max_tokens,
            permissions: Permissions::resolve(builtin, &config.tools),
            teams: Mutex::default(),
        })
    }

    pub fn machine(&self) -> &str {
        self.access.machine()
    }

    /// The subagents of conversation `thread`, which a new team gives
    /// `permissions`, else those this machine's config gives.
    pub fn team(&self, thread: &SessionId, permissions: Option<Arc<Permissions>>) -> Arc<Team> {
        let mut teams = lock(&self.teams);
        let team = teams.entry(thread.clone()).or_insert_with(|| {
            let permissions = permissions.unwrap_or_else(|| Arc::new(self.permissions.fork()));
            Arc::new(Team::new(permissions))
        });
        team.clone()
    }

    pub fn forget(&self, thread: &SessionId) {
        lock(&self.teams).remove(thread);
    }

    /// Runs `task` on this machine, once the directory it asks for is
    /// admitted, reporting its tool calls and asking its questions through
    /// `reporter`. Gives the agent's final reply.
    pub async fn run(&self, team: &Team, task: &Task, reporter: impl Reporter) -> TaskResult {
        if task.follow_up && !team.remembers(&task.agent) {
            return Err(format!(
                "agent {} has no conversation on {} any more; start a new one",
                task.agent,
                self.machine()
            ));
        }
        let dir = self.admit(team, task.directory.as_deref(), &reporter).await?;
        let model = Model::connect(&task.model, &self.providers).map_err(|error| format!("{error:#}"))?;
        let agent = self.agent(team, &task.agent, &model, dir, reporter);
        let runner = agent
            .prompt(task.message.clone())
            .conversation(task.agent.as_str());
        let runner = match task.thinking {
            Some(thinking) => runner.options(GenerationOptions::default().reasoning(thinking)),
            None => runner,
        };
        runner
            .await
            .map(|response| response.output())
            .map_err(|error| error.to_string())
    }

    /// The directory `requested` names, once its rules or the user allow
    /// subagents into it.
    async fn admit(
        &self,
        team: &Team,
        requested: Option<&str>,
        reporter: &impl Reporter,
    ) -> Result<PathBuf, String> {
        let dir = resolve(requested)?;
        let machine = self.machine();
        let _admitting = team.admitting.lock().await;
        match self.access.verdict(&dir, &team.granted()) {
            Verdict::Allowed => return Ok(dir),
            Verdict::Excluded(excluded) => {
                return Err(format!(
                    "{} is in {}, which the user's configuration keeps subagents out of on {machine}",
                    dir.display(),
                    excluded.display()
                ));
            }
            Verdict::Ask => {}
        }
        let ask = Ask::Directory {
            machine: machine.to_owned(),
            path: dir.display().to_string(),
        };
        match reporter.ask(ask).await {
            Decision::Once => {}
            Decision::Always => {
                if let Err(error) = self.access.allow_always(&dir) {
                    let text = format!("{} allowed for this conversation only: {error:#}", dir.display());
                    reporter.emit(Event::Notice { text }).await;
                }
            }
            Decision::Decline => {
                return Err(format!(
                    "The user did not let subagents work in {} on {machine}.",
                    dir.display()
                ));
            }
            Decision::Stop => {
                reporter.stop().await;
                return Err("The user stopped the run.".into());
            }
        }
        team.grant(dir.clone());
        Ok(dir)
    }

    fn agent(
        &self,
        team: &Team,
        agent: &AgentId,
        model: &Model,
        dir: PathBuf,
        reporter: impl Reporter,
    ) -> Agent {
        let environment = environment(&dir, self.machine());
        let place = Place::within(dir, Arc::new(Fenced::new(self.access.excluded())));
        let mut builder = agent::builder(model, &team.seen(agent), place)
            .name("hey subagent")
            .preamble(PROMPT)
            .append_preamble(&environment)
            .default_max_turns(self.max_turns)
            .memory(team.store.clone())
            .add_hook(Approval {
                permissions: team.permissions.clone(),
                reporter: Arc::new(reporter),
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
            granted: Mutex::default(),
            admitting: tokio::sync::Mutex::default(),
            store: MemoryStore::default(),
            seen: Mutex::default(),
            assignments: Mutex::default(),
        }
    }

    pub fn assign(&self, agent: AgentId, assignment: Assignment) {
        lock(&self.assignments).insert(agent, assignment);
    }

    pub fn assignment(&self, agent: &AgentId) -> Option<Assignment> {
        lock(&self.assignments).get(agent).cloned()
    }

    fn granted(&self) -> Vec<PathBuf> {
        lock(&self.granted).clone()
    }

    fn grant(&self, dir: PathBuf) {
        lock(&self.granted).push(dir);
    }

    /// Whether `agent` said anything here before.
    fn remembers(&self, agent: &AgentId) -> bool {
        rig_memory::load(&self.store, agent.as_str())
            .is_ok_and(|conversation| !conversation.messages().is_empty())
    }

    fn seen(&self, agent: &AgentId) -> Seen {
        lock(&self.seen).entry(agent.clone()).or_default().clone()
    }
}

/// Where a subagent works, for the end of its system prompt.
fn environment(dir: &Path, machine: &str) -> String {
    format!(
        "\nEnvironment:\n- Machine: {machine} ({}, {})\n- Working directory: {}\n- Date: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        dir.display(),
        chrono::Local::now().format("%Y-%m-%d"),
    )
}

/// The directory `requested` names: absolute, under `~`, or relative to
/// the working directory, which `None` is. Variables are not expanded, so
/// the model cannot read the environment through the error.
fn resolve(requested: Option<&str>) -> Result<PathBuf, String> {
    let cwd = agent::working_directory();
    let path = match requested.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => cwd.join(shellexpand::tilde(dir).as_ref()),
        None => cwd,
    };
    let dir = path
        .canonicalize()
        .map_err(|error| format!("cannot use {}: {error}", path.display()))?;
    match dir.is_dir() {
        true => Ok(dir),
        false => Err(format!("{} is not a directory", dir.display())),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
