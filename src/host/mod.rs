/* -------------------------------------------------------------------------- */
/*                                host/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod attach;
mod run;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use futures::StreamExt;
use futures::stream::BoxStream;
use rig_agent::agent::{Agent, StreamingResult};
use rig_core::completion::GenerationOptions;
use rig_tools::{Live, Seen};
use tokio_util::task::TaskTracker;

use crate::agent::{self, BUILTIN_TOOLS, Model};
use crate::config::{Config, Providers};
use crate::mcp;
use crate::memory::{self, Memory, Recalled};
use crate::permissions::{Approval, Origin, Permissions};
use crate::protocol::{
    ApprovalId, Decision, Defaults, Ending, Event, ModelInfo, RunOptions, RunRequest, ServerReport,
    Snapshot, Tools,
};
use crate::render;
use crate::sessions::{SessionId, Sessions, Summary};

pub use run::Run;

pub type Events = BoxStream<'static, Event>;

/// Runs the agent and keeps its threads: in-process for a local `hey`, or
/// behind `hey --serve` for clients on other machines. Runs belong to the
/// host, so they carry on whether or not a client is following them.
pub struct Host {
    defaults: Defaults,
    system_prompt: String,
    max_turns: usize,
    max_tokens: Option<u64>,
    providers: Providers,
    sessions: Sessions,
    /// The repository `@shared:` mentions attach from.
    shared: PathBuf,
    memory: Option<Memory>,
    /// The last time memory failed, until a client is told.
    memory_failure: Mutex<Option<String>>,
    mcp: Vec<mcp::Tool>,
    servers: Vec<ServerReport>,
    permissions: Permissions,
    threads: Mutex<HashMap<SessionId, Thread>>,
    /// Runs and what they leave to do once answered, such as learning.
    tasks: TaskTracker,
}

/// What a thread keeps on the host between runs.
struct Thread {
    seen: Seen,
    permissions: Arc<Permissions>,
    /// Recalled as the thread started here and kept, so the preamble stays
    /// the same and the provider's prompt cache keeps working.
    memories: Vec<String>,
    run: Option<Arc<Run>>,
}

/// A thread as saved, and the events of its run in progress.
pub struct Opened {
    pub snapshot: Snapshot,
    pub events: Option<Events>,
}

#[derive(Debug)]
pub enum StartError {
    Busy,
    Refused(anyhow::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Busy => f.write_str("this conversation is already answering"),
            StartError::Refused(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for StartError {}

impl Host {
    /// Opens the sessions store and connects every MCP server.
    pub async fn new(config: &Config) -> Result<Self> {
        let sessions = Sessions::open(config.sessions.dir()?)?;
        let (mcp, servers) = connect_mcp(config).await;
        let builtin = BUILTIN_TOOLS.map(|name| (name, &Origin::Builtin));
        let external = mcp.iter().map(|tool| (tool.name.as_str(), &tool.origin));
        let permissions = Permissions::resolve(builtin.into_iter().chain(external), &config.tools);
        Ok(Self {
            defaults: Defaults {
                model: config.model.clone(),
                thinking: config.thinking,
            },
            system_prompt: config
                .system_prompt
                .clone()
                .unwrap_or_else(|| agent::DEFAULT_SYSTEM_PROMPT.into()),
            max_turns: config.max_turns,
            max_tokens: config.max_tokens,
            providers: config.providers.clone(),
            sessions,
            shared: config.shared.dir()?,
            memory: Memory::open(&config.memory, &config.providers)?,
            memory_failure: Mutex::default(),
            mcp,
            servers,
            permissions,
            threads: Mutex::default(),
            tasks: TaskTracker::new(),
        })
    }

    pub fn defaults(&self) -> Defaults {
        self.defaults.clone()
    }

    pub fn model(&self, reference: &str) -> Result<ModelInfo> {
        Model::connect(reference, &self.providers).map(|model| model.info())
    }

    pub fn models(&self) -> Vec<String> {
        agent::available_models()
    }

    pub fn threads(&self) -> Result<Vec<Summary>> {
        self.sessions.list()
    }

    pub fn open(&self, id: &SessionId) -> Result<Opened> {
        let threads = self.lock();
        let run = threads.get(id).and_then(|thread| thread.run.clone());
        let mut messages = self.sessions.messages(id)?;
        if let Some(run) = &run {
            // The run may have saved already, but its events tell of it until it ends.
            messages.truncate(run.history());
        }
        Ok(Opened {
            snapshot: Snapshot {
                messages,
                running: run.as_ref().map(|run| run.running()),
            },
            events: run.map(|run| run.events().boxed()),
        })
    }

    /// Starts answering `request` in thread `id`, returning the run's events.
    /// A thread new to this host first recalls what it should know.
    pub async fn start(
        self: &Arc<Self>,
        id: SessionId,
        request: RunRequest,
    ) -> Result<Events, StartError> {
        let model = Model::connect(&request.options.model, &self.providers)
            .map_err(StartError::Refused)?;
        let known = self.lock().contains_key(&id);
        let memories = match known {
            true => Vec::new(),
            false => self.recall(&request.prompt).await,
        };
        let mut threads = self.lock();
        let thread = threads.entry(id.clone()).or_insert_with(|| Thread {
            seen: Seen::default(),
            permissions: Arc::new(self.permissions.fork()),
            memories,
            run: None,
        });
        if thread.run.is_some() {
            return Err(StartError::Busy);
        }
        let history = self
            .sessions
            .messages(&id)
            .map_err(StartError::Refused)?
            .len();
        let run = Arc::new(Run::new(request.prompt.clone(), history));
        if let Some(failure) = self.take_memory_failure() {
            run.emit(Event::Notice { text: failure });
        }
        let agent = self.agent(thread, &model, &request.options, &run);
        let events = run.events().boxed();
        thread.run = Some(run.clone());
        drop(threads);
        self.tasks
            .spawn(self.clone().drive(id, run, agent, model, request));
        Ok(events)
    }

    /// False when the approval is not waiting.
    pub fn answer(&self, id: &SessionId, approval: ApprovalId, decision: Decision) -> bool {
        self.run(id)
            .is_some_and(|run| run.answer(approval, decision))
    }

    /// False when the thread is not answering.
    pub fn stop(&self, id: &SessionId) -> bool {
        self.run(id).inspect(|run| run.stop()).is_some()
    }

    pub fn tools(&self, id: &SessionId) -> Tools {
        let permissions = match self.lock().get(id) {
            Some(thread) => thread.permissions.snapshot(),
            None => self.permissions.snapshot(),
        };
        Tools {
            permissions,
            servers: self.servers.clone(),
        }
    }

    /// Every memory, oldest first.
    pub fn memories(&self) -> Result<Vec<String>> {
        self.memory()?.all()
    }

    /// False when `memory` was not saved.
    pub fn forget(&self, memory: &str) -> Result<bool> {
        self.memory()?.forget(memory)
    }

    /// Waits for runs to finish learning, which a host in a process that is
    /// about to exit must, and reports a failure no run reported. Starts no
    /// more runs.
    pub async fn settle(&self) {
        self.tasks.close();
        if !self.tasks.is_empty() {
            render::notice("remembering…");
        }
        self.tasks.wait().await;
        if let Some(failure) = self.take_memory_failure() {
            render::notice(&failure);
        }
    }

    fn agent(&self, thread: &Thread, model: &Model, options: &RunOptions, run: &Arc<Run>) -> Agent {
        let live = Live::new({
            let run = run.clone();
            move |text| {
                run.emit(Event::Output {
                    text: text.to_owned(),
                })
            }
        });
        let system_prompt = options
            .system_prompt
            .as_deref()
            .unwrap_or(&self.system_prompt);
        let mut builder = agent::builder(model, &thread.seen, live)
            .dynamic_tools(self.mcp.iter().map(|tool| tool.tool.clone()).collect())
            .name("hey")
            .preamble(system_prompt)
            .append_preamble(&agent::environment())
            .default_max_turns(self.max_turns)
            .memory(self.sessions.store())
            .add_hook(Approval {
                permissions: thread.permissions.clone(),
                run: run.clone(),
            });
        if let Some(memories) = memory::preamble(&thread.memories) {
            builder = builder.append_preamble(&memories);
        }
        if let Some(max_tokens) = self.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }

    /// Answers `request` into `run` until it ends or is stopped, then learns
    /// from an answer once its clients are let go. The agent saves the thread
    /// as its stream ends.
    async fn drive(
        self: Arc<Self>,
        id: SessionId,
        run: Arc<Run>,
        agent: Agent,
        model: Model,
        request: RunRequest,
    ) {
        let ending = tokio::select! {
            ending = self.respond(&id, &run, &agent, &model, request) => ending,
            () = run.stopped() => Ending::Interrupted,
        };
        let answered = matches!(ending, Ending::Answered);
        {
            // Under the lock, so `open` sees either the run or all it saved.
            let mut threads = self.lock();
            run.emit(Event::End { ending });
            if let Some(thread) = threads.get_mut(&id) {
                thread.run = None;
            }
        }
        if answered && let Err(error) = self.learn(&id, run.history(), &model).await {
            *self.lock_memory_failure() = Some(format!("memory not updated: {error:#}"));
        }
    }

    async fn respond(
        &self,
        id: &SessionId,
        run: &Run,
        agent: &Agent,
        model: &Model,
        request: RunRequest,
    ) -> Ending {
        let RunRequest {
            prompt,
            attachments,
            options,
        } = request;
        let message = self.message(&prompt, &attachments, model, run).await;
        let runner = agent.prompt(message).conversation(id.as_str());
        let mut stream: StreamingResult = match options.thinking {
            Some(thinking) => runner.options(GenerationOptions::default().reasoning(thinking)),
            None => runner,
        }
        .stream();
        loop {
            match stream.next().await {
                None => return Ending::Answered,
                Some(Err(error)) => {
                    return Ending::Failed {
                        error: error.to_string(),
                    };
                }
                Some(Ok(item)) => {
                    if let Some(event) = run::event(item) {
                        run.emit(event);
                    }
                }
            }
        }
    }

    /// Learns from what the thread's run said after its first `history`
    /// messages.
    async fn learn(&self, id: &SessionId, history: usize, model: &Model) -> Result<()> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let messages = self.sessions.messages(id)?;
        let exchange = messages.get(history..).unwrap_or_default();
        let configured = memory
            .model()
            .map(|reference| Model::connect(reference, &self.providers))
            .transpose()?;
        memory.learn(configured.as_ref().unwrap_or(model), exchange).await
    }

    /// The memories a thread opening with `prompt` starts with. A failure is
    /// told with the run.
    async fn recall(&self, prompt: &str) -> Vec<String> {
        let Some(memory) = &self.memory else {
            return Vec::new();
        };
        let (memories, failure) = match memory.recall(prompt).await {
            Ok(Recalled {
                memories,
                fallback,
            }) => (
                memories,
                fallback.map(|error| format!("recalled the newest memories: {error:#}")),
            ),
            Err(error) => (Vec::new(), Some(format!("no memories recalled: {error:#}"))),
        };
        if failure.is_some() {
            *self.lock_memory_failure() = failure;
        }
        memories
    }

    fn memory(&self) -> Result<&Memory> {
        self.memory
            .as_ref()
            .context("memory is off in the host's config (`memory.enabled`)")
    }

    fn take_memory_failure(&self) -> Option<String> {
        self.lock_memory_failure().take()
    }

    fn lock_memory_failure(&self) -> MutexGuard<'_, Option<String>> {
        self.memory_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn run(&self, id: &SessionId) -> Option<Arc<Run>> {
        self.lock().get(id).and_then(|thread| thread.run.clone())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Thread>> {
        self.threads.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The tools of every server that connected, and how each connection went.
async fn connect_mcp(config: &Config) -> (Vec<mcp::Tool>, Vec<ServerReport>) {
    let mut tools = Vec::new();
    let mut reports = Vec::new();
    for server in mcp::connect_all(&config.mcp_servers).await {
        let outcome = server.tools.map(|server_tools| {
            let count = server_tools.len();
            tools.extend(server_tools);
            count
        });
        if let Err(error) = &outcome {
            render::notice(&format!(
                "MCP server `{}` unavailable: {error:#}",
                server.name
            ));
        }
        reports.push(ServerReport {
            name: server.name,
            tools: outcome.map_err(|error| format!("{error:#}")),
        });
    }
    (tools, reports)
}
