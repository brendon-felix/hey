/* -------------------------------------------------------------------------- */
/*                                host/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod attach;
mod run;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::stream::BoxStream;
use futures::{Stream, StreamExt, stream};
use rig_agent::agent::{Agent, StreamingResult};
use rig_core::completion::GenerationOptions;
use rig_tools::{Live, Seen};
use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use crate::agent::{self, Model, Place};
use crate::config::{Config, Providers};
use crate::mcp;
use crate::memory::{self, Memory, Recalled};
use crate::permissions::{Approval, Origin, Permissions};
use crate::preamble::{Hosting, Setting};
use crate::protocol::{
    Activities, Activity, ApprovalId, Ask, Decision, Defaults, DelegationId, Ending, Event,
    Machine, ModelInfo, RunId, RunRequest, ServerReport, Snapshot, TaskResult, ThreadInfo,
    ThreadStatus, Tools,
};
use crate::render;
use crate::results::ResultLimit;
use crate::sessions::{Mark, SessionId, Sessions, Summary};
use crate::skills::{self, Found};
use crate::subagents::{self, Crew, Spawn};
use crate::web::Web;

pub use run::Run;

pub type Events = BoxStream<'static, Event>;

/// How often a stream to a client says something when nothing happens, so a
/// client can tell a quiet host from a lost connection, and proxies between
/// them keep it open.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

/// Runs the agent and keeps its threads: in-process for a local `hey`, or
/// behind `hey --serve` for clients on other machines. Runs belong to the
/// host, so they carry on whether or not a client is following them.
pub struct Host {
    hosting: Hosting,
    /// This machine, as the model is told of it.
    machine: Machine,
    defaults: Defaults,
    system_prompt: String,
    max_turns: usize,
    max_tokens: Option<u64>,
    result_bytes: usize,
    web: Web,
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
    /// Runs subagents here, when the config offers them.
    crew: Option<Arc<Crew>>,
    /// The model subagents use, when not their parent's.
    subagent_model: Option<String>,
    threads: Mutex<HashMap<SessionId, Thread>>,
    /// What the threads are doing, published at every change.
    activity: watch::Sender<Activities>,
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
    /// The latest run, going or not.
    last: Option<RunId>,
}

/// What a thread is doing: its run's state while it has one, else what
/// its mark says.
fn status(run: Option<&Run>, mark: Mark) -> ThreadStatus {
    match run {
        Some(run) if run.asking() => ThreadStatus::Asking,
        Some(_) => ThreadStatus::Running,
        None if mark.unseen => ThreadStatus::Unseen,
        None => ThreadStatus::Idle,
    }
}

/// A thread as saved, and the events of its run in progress.
pub struct Opened {
    pub snapshot: Snapshot,
    pub events: Option<Events>,
}

#[derive(Debug)]
pub enum ThreadError {
    Busy,
    Other(anyhow::Error),
}

impl fmt::Display for ThreadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadError::Busy => f.write_str("this conversation is answering"),
            ThreadError::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for ThreadError {}

impl Host {
    /// Opens the sessions store and connects every MCP server.
    pub async fn new(config: &Config, hosting: Hosting) -> Result<Self> {
        let sessions = Sessions::open(config.sessions.dir()?)?;
        let (mcp, servers) = connect_mcp(config).await;
        let crew = match config.subagents.enabled {
            true => Some(Arc::new(Crew::new(config)?)),
            false => None,
        };
        let web = Web::new(config.tools.result_bytes)?;
        let spawn = crew.as_ref().map(|_| (subagents::TOOL, &Origin::Builtin));
        let builtin = agent::builtin_tools(&web).map(|name| (name, &Origin::Builtin));
        let external = mcp.iter().map(|tool| (tool.name.as_str(), &tool.origin));
        let tools = builtin.chain(spawn).chain(external);
        let permissions = Permissions::resolve(tools, &config.tools);
        Ok(Self {
            hosting,
            machine: Machine::here(config.file.as_deref()),
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
            result_bytes: config.tools.result_bytes,
            web,
            providers: config.providers.clone(),
            sessions,
            shared: config.shared.dir()?,
            memory: Memory::open(&config.memory, &config.providers)?,
            memory_failure: Mutex::default(),
            mcp,
            servers,
            permissions,
            crew,
            subagent_model: config.subagents.model.clone(),
            threads: Mutex::default(),
            activity: watch::Sender::default(),
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

    /// Every conversation, newest first: those saved, and those whose first
    /// answer is still running.
    pub fn threads(&self) -> Result<Vec<ThreadInfo>> {
        let mut summaries = self.sessions.list()?;
        let marks = self.sessions.marks()?;
        let threads = self.lock();
        let unsaved: Vec<Summary> = threads
            .iter()
            .filter_map(|(id, thread)| Some((id, thread.run.as_ref()?)))
            .filter(|(id, _)| !summaries.iter().any(|summary| &summary.id == *id))
            .map(|(id, run)| Summary {
                id: id.clone(),
                title: rig_memory::title_of(run.prompt()),
            })
            .collect();
        summaries.extend(unsaved);
        summaries.sort_unstable_by(|a, b| b.id.cmp(&a.id));
        Ok(summaries
            .into_iter()
            .map(|summary| {
                let mark = marks.get(&summary.id).copied().unwrap_or_default();
                let run = threads.get(&summary.id).and_then(|thread| thread.run.as_deref());
                ThreadInfo {
                    summary,
                    status: status(run, mark),
                    archived: mark.archived,
                }
            })
            .collect())
    }

    /// What the threads are doing now, then again each time that changes.
    pub fn activity(&self) -> impl Stream<Item = Activities> + Send + 'static {
        self.publish(&self.lock());
        let activity = self.activity.subscribe();
        stream::unfold((activity, true), |(mut activity, first)| async move {
            if !first {
                tokio::select! {
                    changed = activity.changed() => changed.ok()?,
                    () = tokio::time::sleep(HEARTBEAT) => {}
                }
            }
            let now = activity.borrow_and_update().clone();
            Some((now, (activity, false)))
        })
    }

    /// A client showed the thread up to its latest answer.
    pub fn viewed(&self, id: &SessionId) -> Result<()> {
        self.sessions.mark(id, |mark| mark.unseen = false)?;
        self.publish(&self.lock());
        Ok(())
    }

    pub fn archive(&self, id: &SessionId, archived: bool) -> Result<()> {
        self.sessions.mark(id, |mark| mark.archived = archived)?;
        self.publish(&self.lock());
        Ok(())
    }

    /// Starts a new thread as a copy of what `id` saved, returning its id.
    pub fn fork(&self, id: &SessionId) -> Result<SessionId> {
        self.sessions.fork(id)
    }

    /// Removes the thread, unless it is answering.
    pub fn delete(&self, id: &SessionId) -> Result<(), ThreadError> {
        let mut threads = self.lock();
        if threads.get(id).is_some_and(|thread| thread.run.is_some()) {
            return Err(ThreadError::Busy);
        }
        threads.remove(id);
        if let Some(crew) = &self.crew {
            crew.forget(id);
        }
        self.sessions.delete(id).map_err(ThreadError::Other)?;
        self.publish(&threads);
        Ok(())
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
    ) -> Result<Events, ThreadError> {
        let model = Model::connect(&request.options.model, &self.providers)
            .map_err(ThreadError::Other)?;
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
            last: None,
        });
        if thread.run.is_some() {
            return Err(ThreadError::Busy);
        }
        let history = self
            .sessions
            .messages(&id)
            .map_err(ThreadError::Other)?
            .len();
        // Whether a tool call waits shows in the activity, as it changes.
        let host = Arc::downgrade(self);
        let asking = move || {
            if let Some(host) = host.upgrade() {
                host.publish(&host.lock());
            }
        };
        let run = Arc::new(Run::new(request.prompt.clone(), history, asking));
        if let Some(failure) = self.take_memory_failure() {
            run.emit(Event::Notice { text: failure });
        }
        let skills = self.skills();
        let agent = self.agent(&id, thread, &model, &request, &skills, &run);
        let events = run.events().boxed();
        thread.run = Some(run.clone());
        thread.last = Some(run.id());
        self.publish(&threads);
        drop(threads);
        self.tasks
            .spawn(self.clone().drive(id, run, agent, model, request, skills));
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

    /// Takes up a subagent's task for a client; false when another took it
    /// first, or it is no longer waited for.
    pub fn claim(&self, id: &SessionId, delegation: DelegationId) -> bool {
        self.run(id).is_some_and(|run| run.claim(delegation))
    }

    /// Shows `event` of a client's subagent as part of the run; false when
    /// the task is no longer waited for.
    pub fn delegated_event(&self, id: &SessionId, delegation: DelegationId, event: Event) -> bool {
        let Some(run) = self.run(id) else {
            return false;
        };
        let Some(claimed) = run.claimed(delegation) else {
            return false;
        };
        run.emit(Event::Agent {
            agent: claimed.agent,
            step: Box::new(event),
        });
        true
    }

    /// Asks the run's clients `ask` for a client's subagent; `None` when the
    /// task is no longer waited for.
    pub async fn delegated_ask(
        &self,
        id: &SessionId,
        delegation: DelegationId,
        ask: Ask,
    ) -> Option<Decision> {
        let run = self.run(id)?;
        let claimed = run.claimed(delegation)?;
        Some(run.ask(subagents::labelled(ask, &claimed.label)).await)
    }

    /// Ends a client's subagent task; false when it was not waited for.
    pub fn finish(&self, id: &SessionId, delegation: DelegationId, result: TaskResult) -> bool {
        self.run(id)
            .is_some_and(|run| run.finish(delegation, result))
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

    /// The skills the agent has now, found from where it works.
    pub fn skills(&self) -> Found {
        skills::find(&agent::working_directory())
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

    fn agent(
        &self,
        id: &SessionId,
        thread: &Thread,
        model: &Model,
        request: &RunRequest,
        skills: &Found,
        run: &Arc<Run>,
    ) -> Agent {
        let live = Live::new({
            let run = run.clone();
            move |text| {
                run.emit(Event::Output {
                    text: text.to_owned(),
                })
            }
        });
        let system_prompt = request
            .options
            .system_prompt
            .as_deref()
            .unwrap_or(&self.system_prompt);
        let spill = agent::spill_dir();
        let setting = Setting {
            hosting: self.hosting,
            host: &self.machine,
            client: request.client.as_ref(),
            model: &request.options.model,
            started: id.started(),
            thread: id,
            sessions: self.sessions.dir(),
            shared: &self.shared,
            memories: self.memory.as_ref().map(Memory::file),
            skills: &skills.roots,
            spill: spill.as_deref(),
            subagents: self.crew.is_some(),
        };
        let builder = agent::builder(model, &thread.seen, Place::here(live), &self.web)
            .dynamic_tools(self.mcp.iter().map(|tool| tool.tool.clone()).collect())
            .name("hey")
            .preamble(system_prompt)
            .append_preamble(&setting.render());
        let mut builder = skills
            .equip(builder)
            .default_max_turns(self.max_turns)
            .memory(self.sessions.store())
            .add_hook(Approval {
                permissions: thread.permissions.clone(),
                reporter: run.clone(),
            })
            .add_hook(ResultLimit {
                bytes: self.result_bytes,
                spill: agent::spill(),
            });
        if let Some(crew) = &self.crew {
            builder = builder.tool(self.spawn(crew, id, thread, request, run));
        }
        if let Some(memories) = memory::preamble(&thread.memories) {
            builder = builder.append_preamble(&memories);
        }
        if let Some(max_tokens) = self.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }

    /// The tool that starts subagents for `run`, sharing the thread's tool
    /// permissions, on the model the config gives them or the run's.
    fn spawn(
        &self,
        crew: &Arc<Crew>,
        id: &SessionId,
        thread: &Thread,
        request: &RunRequest,
        run: &Arc<Run>,
    ) -> Spawn {
        Spawn {
            crew: crew.clone(),
            team: crew.team(id, Some(thread.permissions.clone())),
            run: run.clone(),
            client: request.client.clone(),
            model: self
                .subagent_model
                .clone()
                .unwrap_or_else(|| request.options.model.clone()),
            thinking: request.options.thinking,
        }
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
        skills: Found,
    ) {
        let ending = tokio::select! {
            ending = self.respond(&id, &run, &agent, &model, request, &skills) => ending,
            () = run.stopped() => Ending::Interrupted,
        };
        let answered = matches!(ending, Ending::Answered);
        let saved = self
            .sessions
            .messages(&id)
            .map_or(run.history(), |messages| messages.len());
        {
            // Under the lock, so `open` sees either the run or all it saved,
            // and the thread is marked before any client hears it ended.
            let mut threads = self.lock();
            if let Err(error) = self.sessions.mark(&id, |mark| mark.unseen = true) {
                run.emit(Event::Notice {
                    text: format!("not marked as new: {error:#}"),
                });
            }
            run.emit(Event::End { ending, saved });
            if let Some(thread) = threads.get_mut(&id) {
                thread.run = None;
            }
            self.publish(&threads);
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
        skills: &Found,
    ) -> Ending {
        let RunRequest {
            prompt,
            attachments,
            options,
            ..
        } = request;
        let message = self.message(&prompt, &attachments, skills, model, run).await;
        let runner = agent.prompt(message).conversation(id.as_str());
        let mut stream: StreamingResult = match options.thinking {
            Some(thinking) => runner.options(GenerationOptions::default().reasoning(thinking)),
            None => runner,
        }
        .stream();
        let mut steps = run::Steps::default();
        loop {
            match stream.next().await {
                None => return Ending::Answered,
                Some(Err(error)) => {
                    return Ending::Failed {
                        error: error.to_string(),
                    };
                }
                Some(Ok(item)) => steps.events(item).for_each(|event| run.emit(event)),
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

    /// Tells clients what `threads` are doing now, if that changed: those
    /// that ran since the host started, and those with something new.
    fn publish(&self, threads: &HashMap<SessionId, Thread>) {
        let marks = self.sessions.marks().unwrap_or_default();
        let mark = |id: &SessionId| marks.get(id).copied().unwrap_or_default();
        let ran = threads.iter().map(|(id, thread)| {
            let activity = Activity {
                run: thread.last,
                status: status(thread.run.as_deref(), mark(id)),
                archived: mark(id).archived,
            };
            (id.clone(), activity)
        });
        let unseen = marks
            .iter()
            .filter(|(id, mark)| mark.unseen && !threads.contains_key(*id))
            .map(|(id, mark)| {
                let activity = Activity {
                    run: None,
                    status: ThreadStatus::Unseen,
                    archived: mark.archived,
                };
                (id.clone(), activity)
            });
        let now: Activities = ran.chain(unseen).collect();
        self.activity.send_if_modified(|activity| {
            let changed = *activity != now;
            *activity = now;
            changed
        });
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
