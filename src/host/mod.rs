/* -------------------------------------------------------------------------- */
/*                                host/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod attach;
mod record;
mod run;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::stream::BoxStream;
use futures::{FutureExt, Stream, StreamExt, stream};
use rig_agent::agent::{Agent, StreamingResult};
use rig_core::completion::GenerationOptions;
use rig_core::message::Message;
use rig_memory::CompactReason;
use rig_tools::{Live, Seen};
use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use crate::agent::{self, Model};
use crate::compaction::{self, Applied, Compactor};
use crate::config::{self, Config, Providers, Thinking};
use crate::gate;
use crate::memory::{self, Memory, Recalled};
use crate::permissions::{Origin, Permissions};
use crate::preamble::{Hosting, Setting};
use crate::protocol::{
    Activities, Activity, ApprovalId, Ask, CallId, CallResult, Decision, Defaults, Ending, Event,
    FollowerId, Machine, ModelInfo, Revision, RunId, RunRequest, Snapshot, Task, ThreadInfo,
    ThreadStatus, Tools,
};
use crate::render;
use crate::results::ResultLimit;
use crate::route::Kit;
use crate::sessions::{Mark, SessionId, Sessions, Summary};
use crate::skills::{self, Found};
use crate::steer::{self, Delivery, Steer};
use crate::subagents::{self, Crew, Spawn};
use crate::toolbox::{Toolbox, machine_name};

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
    /// This machine's name, as calls name it.
    name: String,
    defaults: Defaults,
    system_prompt: String,
    max_turns: usize,
    max_tokens: Option<u64>,
    result_bytes: usize,
    toolbox: Arc<Toolbox>,
    providers: Providers,
    sessions: Sessions,
    /// The repository `@shared:` mentions attach from.
    shared: PathBuf,
    memory: Option<Memory>,
    /// The last time memory failed, until a client is told.
    memory_failure: Mutex<Option<String>>,
    compaction: config::Compaction,
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
    /// Recalled as the thread's first answer here started and kept, so the
    /// preamble stays the same and the provider's prompt cache keeps
    /// working.
    memories: Option<Vec<String>>,
    /// Tokens its last model call took in and gave back, to tell when it is
    /// due to compact; unknown once it was compacted after.
    context: Option<u64>,
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

/// What a run answering a request works with, besides its agent.
struct Answering {
    model: Model,
    request: RunRequest,
    skills: Found,
    live: Arc<compaction::Live>,
}

/// What `work` came to, unless `run` was stopped first.
async fn until_stopped<T>(run: &Run, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        done = work => Some(done),
        () = run.stopped() => None,
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
        let toolbox = Toolbox::new(config).await?;
        let crew = config.subagents.enabled.then(|| Arc::new(Crew::new(config)));
        let spawn = crew.as_ref().map(|_| subagents::TOOL);
        let host_only = [skills::TOOL, compaction::TOOL].into_iter().chain(spawn);
        let host_only = host_only.map(|name| (name, &Origin::Builtin));
        let permissions = Permissions::resolve(toolbox.origins().chain(host_only), &config.tools);
        Ok(Self {
            hosting,
            machine: Machine::here(config.file.as_deref()),
            name: machine_name(),
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
            toolbox: Arc::new(toolbox),
            providers: config.providers.clone(),
            sessions,
            shared: config.shared.dir()?,
            memory: Memory::open(&config.memory, &config.providers)?,
            memory_failure: Mutex::default(),
            compaction: config.compaction.clone(),
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

    /// The thread as saved and its run's events; a client on `machine`
    /// that runs tool calls follows the run while it holds them.
    pub fn open(&self, id: &SessionId, machine: Option<String>) -> Result<Opened> {
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
            events: run.map(|run| run.events(machine).boxed()),
        })
    }

    /// Starts `request` in thread `id`, returning the run's events, which a
    /// client on `machine` that runs tool calls follows the run by. A
    /// thread's first answer here first recalls what it should know.
    pub async fn start(
        self: &Arc<Self>,
        id: SessionId,
        mut request: RunRequest,
        machine: Option<String>,
    ) -> Result<Events, ThreadError> {
        // A client on this machine lends tools the host has already.
        if let Some(client) = &mut request.client
            && client.lent.as_ref().is_some_and(|lent| lent.machine == self.name)
        {
            client.lent = None;
        }
        let model = Model::connect(&request.options.model, &self.providers)
            .map_err(ThreadError::Other)?;
        let compactor = Compactor::new(&self.compaction, &model, &self.providers).map_err(ThreadError::Other)?;
        let recalled = self.lock().get(&id).is_some_and(|thread| thread.memories.is_some());
        let memories = match (recalled, request.task) {
            (false, Task::Answer) => Some(self.recall(&request.prompt).await),
            _ => None,
        };
        let mut threads = self.lock();
        let thread = threads.entry(id.clone()).or_insert_with(|| Thread {
            seen: Seen::default(),
            permissions: Arc::new(self.permissions.fork()),
            memories: None,
            context: None,
            run: None,
            last: None,
        });
        if thread.run.is_some() {
            return Err(ThreadError::Busy);
        }
        if thread.memories.is_none() {
            thread.memories = memories;
        }
        let basis = self.sessions.basis(&id).map_err(ThreadError::Other)?;
        // Whether a tool call waits shows in the activity, as it changes.
        let host = Arc::downgrade(self);
        let asking = move || {
            if let Some(host) = host.upgrade() {
                host.publish(&host.lock());
            }
        };
        let shown = match request.task {
            Task::Answer => request.prompt.clone(),
            Task::Compact => format!("/compact {}", request.prompt).trim_end().to_owned(),
        };
        let run = Arc::new(Run::new(
            shown,
            request.options.model.clone(),
            basis.messages.len(),
            asking,
        ));
        if let Some(failure) = self.take_memory_failure() {
            run.emit(Event::Notice { text: failure });
        }
        let events = run.events(machine).boxed();
        tracing::info!(thread = id.as_str(), run = ?run.id(), model = %request.options.model, task = ?request.task, "run started");
        let work = match request.task {
            Task::Answer => {
                let history = basis.messages.len();
                let live = compaction::Live::new(compactor, run.clone(), history, basis.from, basis.state, thread.context);
                let answering = Answering {
                    model,
                    request,
                    skills: self.skills(),
                    live: Arc::new(live),
                };
                let agent = self.agent(&id, thread, &run, &answering);
                self.clone().drive(id, run.clone(), agent, answering).boxed()
            }
            Task::Compact => {
                run.close_compactions();
                self.clone().drive_compaction(id, run.clone(), compactor, request.prompt).boxed()
            }
        };
        thread.run = Some(run.clone());
        thread.last = Some(run.id());
        self.publish(&threads);
        drop(threads);
        self.tasks.spawn(work);
        Ok(events)
    }

    /// Has the run answering in thread `id` compact before its next model
    /// call, or once it answered; false when the thread is not answering,
    /// or its answer is ending.
    pub fn compact_running(&self, id: &SessionId, focus: &str) -> bool {
        self.run(id).is_some_and(|run| run.compact(focus))
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

    /// Has the model answering in thread `id` handed `steer` as soon as it
    /// can; false when the thread is not answering, or its answer is ending.
    pub async fn steer(&self, id: &SessionId, steer: Steer) -> bool {
        let Some(run) = self.run(id) else {
            return false;
        };
        let steering = self.steering(steer, &run).await;
        run.steer(steering)
    }

    /// Takes up call `call` for `follower`; false when another client took
    /// it first, or it is no longer waited for.
    pub fn claim(&self, id: &SessionId, call: CallId, follower: FollowerId) -> bool {
        self.run(id).is_some_and(|run| run.claim(call, follower))
    }

    /// Shows `event` of a client's call as part of the run, under the
    /// subagent making it; false when the call is no longer waited for.
    pub fn call_event(&self, id: &SessionId, call: CallId, event: Event) -> bool {
        let Some(run) = self.run(id) else {
            return false;
        };
        let Some(claimed) = run.claimed(call) else {
            return false;
        };
        run.emit(match claimed.agent {
            Some(agent) => Event::Agent {
                agent,
                step: Box::new(event),
            },
            None => event,
        });
        true
    }

    /// Asks the run's clients `ask` for a client's call; `None` when the
    /// call is no longer waited for.
    pub async fn call_ask(&self, id: &SessionId, call: CallId, ask: Ask) -> Option<Decision> {
        let run = self.run(id)?;
        run.claimed(call)?;
        Some(run.ask(ask).await)
    }

    /// Ends a client's call; false when it was not waited for.
    pub fn finish(&self, id: &SessionId, call: CallId, result: CallResult) -> bool {
        self.run(id).is_some_and(|run| run.finish(call, result))
    }

    pub fn tools(&self, id: &SessionId) -> Tools {
        let permissions = match self.lock().get(id) {
            Some(thread) => thread.permissions.snapshot(),
            None => self.permissions.snapshot(),
        };
        Tools {
            permissions,
            servers: self.toolbox.servers().to_vec(),
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

    /// False when it is already saved.
    pub fn remember(&self, memory: &str) -> Result<bool> {
        self.memory()?.remember(memory)
    }

    pub fn revise(&self, revision: &Revision) -> Result<()> {
        self.memory()?.revise(&revision.old, &revision.new)
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

    fn agent(&self, id: &SessionId, thread: &Thread, run: &Arc<Run>, answering: &Answering) -> Agent {
        let Answering {
            model,
            request,
            skills,
            live: compaction,
        } = answering;
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
        };
        let kit = Kit {
            toolbox: self.toolbox.clone(),
            host: self.name.clone(),
            lent: request.client.as_ref().and_then(|client| client.lent.clone()),
            run: run.clone(),
        };
        let (tools, routes) = kit.tools(&thread.seen, Some(live), None);
        let builder = agent::builder(model, tools, routes, thread.permissions.clone(), run.clone())
        .name("hey")
        .preamble(system_prompt)
        .append_preamble(&setting.render());
        let mut builder = skills
            .equip(builder)
            .default_max_turns(self.max_turns)
            .memory(self.sessions.store())
            .add_hook(ResultLimit {
                bytes: self.result_bytes,
                spill: agent::spill(),
            })
            .add_hook(run.record().clone())
            .add_hook(Delivery(run.clone()))
            .add_hook(compaction::Hook(compaction.clone()))
            .tool(compaction::Compact { run: run.clone() });
        if let Some(crew) = &self.crew {
            builder = builder.tool(self.spawn(crew, id, thread, request, kit));
        }
        if let Some(memories) = memory::preamble(thread.memories.as_deref().unwrap_or_default()) {
            builder = builder.append_preamble(&memories);
        }
        if let Some(max_tokens) = self.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }

    /// The tool that starts subagents with the run's tools, sharing the
    /// thread's tool permissions, on the model the config gives them or the
    /// run's.
    fn spawn(&self, crew: &Arc<Crew>, id: &SessionId, thread: &Thread, request: &RunRequest, kit: Kit) -> Spawn {
        Spawn {
            crew: crew.clone(),
            team: crew.team(id, thread.permissions.clone()),
            kit,
            model: self
                .subagent_model
                .clone()
                .unwrap_or_else(|| request.options.model.clone()),
            thinking: request.options.thinking,
        }
    }

    /// Answers into `run` until it ends or is stopped, then learns from an
    /// answer once its clients are let go. The agent saves the thread as
    /// each of its streams ends; what a run that did not answer said, and
    /// the summary it compacted to, are saved here.
    async fn drive(self: Arc<Self>, id: SessionId, run: Arc<Run>, agent: Agent, answering: Answering) {
        let Answering {
            model,
            request,
            skills,
            live,
        } = answering;
        let conversing = self.converse(&id, &run, &agent, &model, request, &skills);
        let ending = until_stopped(&run, conversing).await.unwrap_or(Ending::Interrupted);
        run.close_steers();
        self.keep(&id, &run, &ending);
        let answered = matches!(ending, Ending::Answered);
        // Read before a summary takes the place of what the answer said.
        let exchange = (answered && self.memory.is_some()).then(|| {
            let messages = self.sessions.messages(&id)?;
            anyhow::Ok(messages.get(run.history()..).unwrap_or_default().to_vec())
        });
        let mut context = live.context();
        if let Some(applied) = live.applied()
            && !self.save_summary(&id, &run, &applied)
        {
            context = None;
        }
        // Asked as the answer ended, with no model call left to make it.
        if let (Ending::Answered, Some(focus)) = (&ending, run.close_compactions()) {
            context = None;
            if let Some(Err(error)) = until_stopped(&run, self.compact_saved(&id, &run, live.compactor(), focus)).await {
                run.emit(Event::Notice {
                    text: format!("not compacted: {error:#}"),
                });
            }
        }
        self.end(&id, &run, ending, context);
        if let Some(exchange) = exchange
            && let Err(error) = async { self.learn(&exchange?, &model).await }.await
        {
            *self.lock_memory_failure() = Some(format!("memory not updated: {error:#}"));
        }
    }

    /// Compacts the thread as saved into `run`, `focus` saying what the
    /// summary should keep above all.
    async fn drive_compaction(self: Arc<Self>, id: SessionId, run: Arc<Run>, compactor: Compactor, focus: String) {
        let ending = match until_stopped(&run, self.compact_saved(&id, &run, &compactor, focus)).await {
            None => Ending::Interrupted,
            Some(Ok(())) => Ending::Answered,
            Some(Err(error)) => Ending::Failed {
                error: format!("not compacted: {error:#}"),
            },
        };
        self.end(&id, &run, ending, None);
    }

    /// Replaces the older messages of the thread as saved with a summary,
    /// telling `run`'s clients.
    async fn compact_saved(&self, id: &SessionId, run: &Run, compactor: &Compactor, focus: String) -> Result<()> {
        let basis = self.sessions.basis(id)?;
        run.emit(Event::Notice {
            text: "compacting the conversation…".into(),
        });
        let reason = CompactReason::Asked { focus };
        let Some(summary) = compactor.summarize(&basis.messages, basis.from, &basis.state, &reason).await? else {
            run.emit(Event::Notice {
                text: "nothing to compact yet".into(),
            });
            return Ok(());
        };
        let kept = basis.messages.get(summary.upto).context("nothing is kept after the summary")?;
        anyhow::ensure!(
            self.sessions.summarize(id, summary.upto, kept, &summary.state)?,
            "the conversation changed while it was summarized"
        );
        tracing::info!(thread = id.as_str(), tokens = summary.before, "compacted");
        run.emit(summary.event());
        Ok(())
    }

    /// Saves the summary a run compacted to in place of what it replaced;
    /// false when it could not, as when the run's end saved the thread
    /// other than it was answered.
    fn save_summary(&self, id: &SessionId, run: &Run, applied: &Applied) -> bool {
        let failure = match self.sessions.summarize(id, applied.at, &applied.kept, &applied.state) {
            Ok(true) => return true,
            Ok(false) => "the conversation was saved otherwise".to_owned(),
            Err(error) => format!("{error:#}"),
        };
        tracing::warn!(thread = id.as_str(), failure, "cannot save a summary");
        run.emit(Event::Notice {
            text: format!("the summary is not saved, so it is compacted again when due: {failure}"),
        });
        false
    }

    /// Lets clients know `run` ended with `ending`, and the thread is free.
    fn end(&self, id: &SessionId, run: &Run, ending: Ending, context: Option<u64>) {
        tracing::info!(thread = id.as_str(), run = ?run.id(), ?ending, "run ended");
        let saved = self
            .sessions
            .messages(id)
            .map_or(run.history(), |messages| messages.len());
        // Under the lock, so `open` sees either the run or all it saved,
        // and the thread is marked before any client hears it ended.
        let mut threads = self.lock();
        if let Err(error) = self.sessions.mark(id, |mark| mark.unseen = true) {
            run.emit(Event::Notice {
                text: format!("not marked as new: {error:#}"),
            });
        }
        run.emit(Event::End { ending, saved });
        if let Some(thread) = threads.get_mut(id) {
            thread.run = None;
            thread.context = context;
        }
        self.publish(&threads);
    }

    /// Saves what a run that did not answer said, which the agent leaves
    /// unsaved, unless the thread changed since its latest answer began, as
    /// when the agent saved an answer just as the run was stopped.
    fn keep(&self, id: &SessionId, run: &Run, ending: &Ending) {
        let reason = match ending {
            Ending::Answered => return,
            Ending::Interrupted => "the user stopped the reply",
            Ending::Failed { error } => error,
        };
        let said = run.record().said(reason);
        if said.is_empty() {
            return;
        }
        let count = said.len();
        match self.sessions.append(id, run.record().held(), said) {
            Ok(true) => tracing::info!(thread = id.as_str(), messages = count, "saved what an unfinished run said"),
            Ok(false) => tracing::info!(thread = id.as_str(), "an unfinished run's thread changed; saved nothing"),
            Err(error) => {
                tracing::warn!(thread = id.as_str(), error = format!("{error:#}"), "cannot save an unfinished run");
                run.emit(Event::Notice {
                    text: format!("what was said is not saved: {error:#}"),
                });
            }
        }
    }

    /// Answers `request`, then what the user sent meanwhile that the model
    /// was not yet handed, until an answer leaves nothing waiting.
    async fn converse(
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
        let mut message = self.message(&prompt, &attachments, skills, model, run).await;
        loop {
            let ending = self.respond(id, run, agent, message, options.thinking).await;
            let waiting = match ending {
                Ending::Answered => run.last_steers(),
                Ending::Interrupted | Ending::Failed { .. } => Vec::new(),
            };
            if waiting.is_empty() {
                return ending;
            }
            for steering in &waiting {
                run.emit(Event::Steered {
                    prompt: steering.prompt.clone(),
                });
            }
            message = steer::message(waiting);
        }
    }

    async fn respond(
        &self,
        id: &SessionId,
        run: &Run,
        agent: &Agent,
        message: Message,
        thinking: Option<Thinking>,
    ) -> Ending {
        let runner = agent
            .prompt(message)
            .conversation(id.as_str())
            .tool_concurrency(gate::CONCURRENCY);
        let mut stream: StreamingResult = match thinking {
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
                Some(Ok(item)) => steps.events(item).into_iter().for_each(|event| run.emit(event)),
            }
        }
    }

    /// Learns from what a run said: the `exchange` its answer added.
    async fn learn(&self, exchange: &[Message], model: &Model) -> Result<()> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
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
