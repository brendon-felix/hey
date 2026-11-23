/* -------------------------------------------------------------------------- */
/*                                host/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod attach;
mod record;
mod run;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::stream::BoxStream;
use futures::{FutureExt, Stream, StreamExt, stream};
use rig_agent::agent::{Agent, MultiTurnStreamItem, StreamingResult};
use rig_agent::completion::PromptError;
use rig_core::completion::GenerationOptions;
use rig_core::message::{Message, UserContent};
use rig_memory::CompactReason;
use rig_tools::{Live, Seen};
use tokio::sync::watch;
use tokio_util::task::TaskTracker;

use crate::agent::{self, Model};
use crate::checkpoint::{Checkpoints, Turn};
use crate::compaction::{self, Applied, Compactor};
use crate::config::{self, Config, Providers, Thinking};
use crate::gate;
use crate::goal;
use crate::hooks::{HookEvent, Hooks, MAX_STOP_BLOCKS, Scope, ToolHooks};
use crate::instructions::Instructions;
use crate::memory::{self, Memory};
use crate::models;
use crate::naming;
use crate::notes::Notes;
use crate::permissions::{Origin, Permissions};
use crate::preamble::{Hosting, Setting};
use crate::protocol::{
    Activities, Activity, AgentId, AgentInfo, AgentSnapshot, ApprovalId, Ask, CallId, CallResult,
    ClientInfo, Decision, Defaults, Ending, Event, FollowerId, Logged, Machine, ModelInfo, Models,
    Revision, RunId, RunRequest, Snapshot, Task, ThreadInfo, ThreadStatus, Tools, Undone,
};
use crate::render;
use crate::results::ResultLimit;
use crate::retry::{self, Attempts, Backoff};
use crate::route::Kit;
use crate::sessions::{Mark, SessionId, Sessions, Summary};
use crate::skills::{self, Found};
use crate::steer::{self, Delivery, Steer};
use crate::subagents::Kinds;
use crate::subagents::{self, Crew, Spawn};
use crate::toolbox::{Toolbox, machine_name};
use crate::tools;
use crate::transcript;

pub use run::Run;

pub type Events = BoxStream<'static, Logged>;

/// How often a stream to a client says something when nothing happens, so a
/// client can tell a quiet host from a lost connection, and proxies between
/// them keep it open.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

/// How long a client that lost a run's stream can still follow the run
/// again to its end, once it ended.
const RESUMABLE: Duration = Duration::from_secs(300);

/// What the model is asked once what it said before its connection failed
/// is saved.
const CONTINUE: &str =
    "The connection to you failed before you finished. Continue where you left off.";

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
    /// What clients synced, for agents to go on with while they are away.
    sync: crate::sync::Store,
    projects: Arc<crate::projects::Projects>,
    memory: Option<Arc<Memory>>,
    /// The last time memory failed, until a client is told.
    memory_failure: Mutex<Option<String>>,
    compaction: config::Compaction,
    instructions: config::Instructions,
    /// The files each answer changed, as they were, when checkpoints are on.
    checkpoints: Option<Checkpoints>,
    goal: config::Goal,
    hooks: Arc<Hooks>,
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
    /// Set once the host is going down, so the runs it stops say why.
    closing: AtomicBool,
}

/// What a thread keeps on the host between runs.
struct Thread {
    seen: Seen,
    permissions: Arc<Permissions>,
    /// Plan mode and the todo list.
    notes: Arc<Notes>,
    /// Read as the thread's first answer here started and kept, for the
    /// same reason.
    instructions: Option<Instructions>,
    /// Tokens its last model call took in and gave back, to tell when it is
    /// due to compact; unknown once it was compacted after.
    context: Option<u64>,
    run: Option<Arc<Run>>,
    /// The run that last ended, for a client that lost its stream to see
    /// the end of, for [`RESUMABLE`].
    ended: Option<Ended>,
    /// The latest run, going or not.
    last: Option<RunId>,
}

struct Ended {
    run: Arc<Run>,
    at: Instant,
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

/// How long a model may take to name a thread.
const NAMING_TIMEOUT: Duration = Duration::from_secs(60);

impl Host {
    /// Opens the sessions store and connects every MCP server.
    pub async fn new(config: &Config, hosting: Hosting) -> Result<Self> {
        let sessions = Sessions::open(config.sessions.dir()?, config.sessions.markdown_dir()?)?;
        let toolbox = Toolbox::new(config).await?;
        let crew = config
            .subagents
            .enabled
            .then(|| Arc::new(Crew::new(config)));
        let spawn = crew.as_ref().map(|_| subagents::TOOL);
        let memory_tool = config.memory.enabled.then_some(memory::TOOL);
        let host_only = [
            skills::TOOL,
            compaction::TOOL,
            tools::TODO,
            tools::ASK_USER,
            tools::EXIT_PLAN,
            crate::projects::TOOL,
        ]
        .into_iter()
        .chain(spawn)
        .chain(memory_tool);
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
            sync: crate::sync::Store::new(config.sync.dir()?),
            projects: {
                let data = config::data_dir()?;
                crate::projects::install_skill();
                Arc::new(crate::projects::Projects::open(
                    data.join("projects.toml"),
                    data.join("projects"),
                )?)
            },
            memory: Memory::open(&config.memory, &config.providers)?.map(Arc::new),
            memory_failure: Mutex::default(),
            compaction: config.compaction.clone(),
            instructions: config.instructions.clone(),
            checkpoints: match config.checkpoints.enabled {
                true => Some(Checkpoints::new(
                    &config::data_dir()?,
                    agent::working_directory(),
                )),
                false => None,
            },
            goal: config.goal.clone(),
            hooks: Arc::new(Hooks::new(&config.hooks)?),
            permissions,
            crew,
            subagent_model: config.subagents.model.clone(),
            threads: Mutex::default(),
            activity: watch::Sender::default(),
            tasks: TaskTracker::new(),
            closing: AtomicBool::new(false),
        })
    }

    pub fn defaults(&self) -> Defaults {
        self.defaults.clone()
    }

    pub fn model(&self, reference: &str) -> Result<ModelInfo> {
        Model::connect(reference, &self.providers).map(|model| model.info())
    }

    pub async fn models(&self) -> Models {
        models::list(&self.providers).await
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
                let run = threads
                    .get(&summary.id)
                    .and_then(|thread| thread.run.as_deref());
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
        let copy = self.sessions.fork(id)?;
        _ = std::fs::copy(self.notes_file(id), self.notes_file(&copy));
        Ok(copy)
    }

    /// Puts thread `id` in plan mode or takes it out, telling the run
    /// answering in it, if any.
    pub fn set_plan(&self, id: &SessionId, on: bool) {
        let mut threads = self.lock();
        let thread = self.thread(&mut threads, id);
        thread.notes.set_planning(on);
        if let Some(run) = &thread.run {
            run.emit(Event::Plan { on });
        }
    }

    /// Where the notes of thread `id` are saved.
    fn notes_file(&self, id: &SessionId) -> PathBuf {
        self.sessions
            .dir()
            .join("notes")
            .join(format!("{}.json", id.as_str()))
    }

    /// Thread `id` as kept here, kept from now on if it was not.
    fn thread<'a>(
        &self,
        threads: &'a mut HashMap<SessionId, Thread>,
        id: &SessionId,
    ) -> &'a mut Thread {
        threads.entry(id.clone()).or_insert_with(|| {
            let permissions = Arc::new(self.permissions.fork());
            Thread {
                seen: Seen::default(),
                notes: Arc::new(Notes::load(Some(self.notes_file(id)), permissions.clone())),
                permissions,
                instructions: None,
                context: None,
                run: None,
                ended: None,
                last: None,
            }
        })
    }

    /// Where the thread's Markdown is kept up to date, once it has any.
    pub fn markdown(&self, id: &SessionId) -> Option<std::path::PathBuf> {
        self.sessions.markdown(id)
    }

    /// Removes the thread, unless it is answering.
    pub fn delete(&self, id: &SessionId) -> Result<(), ThreadError> {
        let mut threads = self.lock();
        if threads.get(id).is_some_and(|thread| thread.run.is_some()) {
            return Err(ThreadError::Busy);
        }
        threads.remove(id);
        if let Some(checkpoints) = &self.checkpoints {
            checkpoints.forget(id);
        }
        _ = std::fs::remove_file(self.notes_file(id));
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
        let mut threads = self.lock();
        let thread = self.thread(&mut threads, id);
        let run = thread.run.clone();
        let plan = thread.notes.planning();
        let mut messages = self.sessions.messages(id)?;
        if let Some(run) = &run {
            // The run may have saved already, but its events tell of it until it ends.
            messages.truncate(run.history());
        }
        Ok(Opened {
            snapshot: Snapshot {
                messages,
                running: run.as_ref().map(|run| run.running()),
                plan,
            },
            events: run.map(|run| run.events(machine, 0).boxed()),
        })
    }

    /// The events of run `run` of thread `id` after its first `after`, for
    /// a client that lost its stream of them; `None` once the run is gone.
    pub fn follow(
        &self,
        id: &SessionId,
        run: RunId,
        after: usize,
        machine: Option<String>,
    ) -> Option<Events> {
        let threads = self.lock();
        let thread = threads.get(id)?;
        let ended = thread.ended.as_ref().map(|ended| &ended.run);
        let found = thread
            .run
            .iter()
            .chain(ended)
            .find(|found| found.id() == run)?;
        Some(found.events(machine, after).boxed())
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
            && client
                .lent
                .as_ref()
                .is_some_and(|lent| lent.machine == self.name)
        {
            client.lent = None;
        }
        let model =
            Model::connect(&request.options.model, &self.providers).map_err(ThreadError::Other)?;
        let compactor = Compactor::new(&self.compaction, &model, &self.providers)
            .map_err(ThreadError::Other)?;
        let mut threads = self.lock();
        let thread = self.thread(&mut threads, &id);
        let compactor = compactor.keeping(thread.notes.clone());
        if thread.run.is_some() {
            return Err(ThreadError::Busy);
        }
        if thread.instructions.is_none() && self.instructions.enabled {
            thread.instructions = Some(Instructions::find(
                &agent::working_directory(),
                self.instructions.max_bytes,
            ));
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
        let events = run.events(machine, 0).boxed();
        tracing::info!(thread = id.as_str(), run = ?run.id(), model = %request.options.model, task = ?request.task, "run started");
        let work = match request.task {
            Task::Answer => {
                let history = basis.messages.len();
                let live = compaction::Live::new(
                    compactor.hooked(self.scope(&id)),
                    run.clone(),
                    history,
                    basis.from,
                    basis.state,
                    thread.context,
                );
                let answering = Answering {
                    model,
                    request,
                    skills: self.skills(),
                    live: Arc::new(live),
                };
                let agent = self.agent(&id, thread, &run, &answering);
                self.clone()
                    .drive(id, run.clone(), agent, answering)
                    .boxed()
            }
            Task::Compact => {
                run.close_compactions();
                let compactor = compactor.hooked(self.scope(&id));
                self.clone()
                    .drive_compaction(id, run.clone(), compactor, request.prompt)
                    .boxed()
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

    /// Puts the files the latest answer in thread `id` that changed any
    /// back as they were before it; `None` when none is left to undo. The
    /// conversation stays as it is.
    pub fn undo(&self, id: &SessionId) -> Result<Option<Undone>, ThreadError> {
        let threads = self.lock();
        if threads.get(id).is_some_and(|thread| thread.run.is_some()) {
            return Err(ThreadError::Busy);
        }
        let checkpoints = self.checkpoints.as_ref().ok_or_else(|| {
            ThreadError::Other(anyhow::anyhow!(
                "checkpoints are off in the host's config (`checkpoints.enabled`)"
            ))
        })?;
        let undone = checkpoints.undo(id).map_err(ThreadError::Other)?;
        drop(threads);
        Ok(undone)
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

    /// Takes back the message `prompt` steering thread `id`; false when the
    /// model was already handed it, or the thread is not answering.
    pub fn unsteer(&self, id: &SessionId, prompt: &str) -> bool {
        self.run(id).is_some_and(|run| run.unsteer(prompt))
    }

    /// Takes up call `call` for `follower`; false when another client took
    /// it first, or it is no longer waited for.
    pub fn claim(&self, id: &SessionId, call: CallId, follower: FollowerId) -> bool {
        self.run(id).is_some_and(|run| run.claim(call, follower))
    }

    /// Shows `event` of a client's call as part of the run, under the
    /// subagent making it; false when the call is no longer waited for.
    pub fn sync(&self) -> &crate::sync::Store {
        &self.sync
    }

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

    /// The kinds of subagent the agent may start, found from where it works.
    pub fn agents(&self) -> Kinds {
        subagents::kinds(&agent::working_directory())
    }

    /// The subagents thread `id` started since the host did.
    pub fn subagents(&self, id: &SessionId) -> Vec<AgentInfo> {
        self.team(id).map(|team| team.roster()).unwrap_or_default()
    }

    /// What subagent `agent` of thread `id` said, and the task it works on.
    pub fn subagent(&self, id: &SessionId, agent: &AgentId) -> Option<AgentSnapshot> {
        self.team(id)?.snapshot(agent)
    }

    fn team(&self, id: &SessionId) -> Option<Arc<subagents::Team>> {
        self.crew.as_ref()?.find(id)
    }

    /// The prompts the host's MCP servers offer.
    pub fn prompts(&self) -> Vec<crate::mcp::Prompt> {
        self.toolbox.prompts().to_vec()
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

    /// Stops every run, so what each said so far is saved to its thread,
    /// and waits for that, for as long as `within`. A host about to go down
    /// does this first, or the turn in progress is lost with the process.
    pub async fn wind_down(&self, within: Duration) {
        self.closing.store(true, Ordering::SeqCst);
        let runs: Vec<_> = self
            .lock()
            .values()
            .filter_map(|thread| thread.run.clone())
            .collect();
        if !runs.is_empty() {
            tracing::info!(runs = runs.len(), "stopping runs to save what they said");
        }
        runs.iter().for_each(|run| run.stop());
        self.tasks.close();
        if tokio::time::timeout(within, self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("runs did not finish saving in time");
        }
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
        run: &Arc<Run>,
        answering: &Answering,
    ) -> Agent {
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
            surface: request.surface,
            model: &request.options.model,
            started: id.started(),
            thread: id,
            sessions: self.sessions.dir(),
            shared: &self.shared,
            synced: self.sync.dir(),
            memories: self.memory.as_ref().map(|memory| memory.file()),
            skills: &skills.roots,
            spill: spill.as_deref(),
        };
        let kit = Kit {
            toolbox: self.toolbox.clone(),
            host: self.name.clone(),
            lent: request
                .client
                .as_ref()
                .and_then(|client| client.lent.clone()),
            client_skills: request
                .client
                .as_ref()
                .map(|client| client.skills.clone())
                .unwrap_or_default(),
            run: run.clone(),
            turn: self.turn(id),
        };
        let (tools, routes) = kit.tools(&thread.seen, Some(live), None);
        let current = request
            .client
            .as_ref()
            .filter(|client| client.lent.is_some())
            .and_then(|client| {
                let machine = client.lent.as_ref()?.machine.clone();
                let info = client.project.as_ref()?;
                let name = self.projects.observe(&machine, info).ok()?;
                Some((name, machine))
            });
        if let Some((name, _)) = &current
            && let Some(project) = self.projects.get(name)
        {
            thread.permissions.overlay(&project.tools);
        }
        let notes = &thread.notes;
        let hooks = ToolHooks::new(self.scope(id), run.clone());
        let builder = agent::builder(
            model,
            tools,
            routes,
            thread.permissions.clone(),
            run.clone(),
            Some(hooks),
        )
        .name("hey")
        .preamble(system_prompt)
        .append_preamble(&setting.render());
        let builder = match &current {
            Some((name, machine)) => {
                match self.projects.note(name, &self.name, machine) {
                    Some(note) => builder.append_preamble(&note),
                    None => builder,
                }
            }
            None => builder,
        };
        let skills_here = skills;
        let builder = skills.equip(builder);
        let builder = match request.client.as_ref() {
            Some(ClientInfo {
                skills,
                lent: Some(lent),
                ..
            }) => {
                let builder = match &lent.deferred {
                    Some(deferred) => builder.append_preamble(
                        &crate::mcp::deferred::client_preamble(&lent.machine, deferred),
                    ),
                    None => builder,
                };
                match skills::client_catalog(&lent.machine, skills, &skills_here.skills) {
                    Some(catalog) => builder.append_preamble(&catalog),
                    None => builder,
                }
            }
            _ => builder,
        };
        let mut builder = builder
            .default_max_turns(self.max_turns)
            .memory(self.sessions.store())
            .add_hook(ResultLimit {
                bytes: self.result_bytes,
                spill: agent::spill(),
            })
            .add_hook(run.record().clone())
            .add_hook(Delivery(run.clone()))
            .add_hook(compaction::Hook(compaction.clone()))
            .add_hook(crate::budget::Budget::new(self.max_turns, Some(run.clone())))
            .tool(compaction::Compact { run: run.clone() })
            .tool(tools::Todos {
                notes: notes.clone(),
            })
            .tool(tools::AskUser { run: run.clone() })
            .tool(tools::ExitPlan {
                run: run.clone(),
                notes: notes.clone(),
            });
        if let Some((name, machine)) = current {
            builder = builder.tool(crate::projects_tool::ProjectTool {
                projects: self.projects.clone(),
                run: run.clone(),
                host: self.name.clone(),
                client: Some(machine),
                current: Some(name),
            });
        }
        if let Some(crew) = &self.crew {
            builder = builder.tool(self.spawn(crew, id, thread, request, kit));
        }
        if let Some(deferred) = self.toolbox.preamble() {
            builder = builder.append_preamble(&deferred);
        }
        if let Some(memory) = &self.memory {
            builder = builder.tool(memory::Recall {
                memory: memory.clone(),
            });
        }
        if let Some(instructions) = thread
            .instructions
            .as_ref()
            .and_then(Instructions::preamble)
        {
            builder = builder.append_preamble(&instructions);
        }
        // Anthropic refuses a request without `max_tokens`, and the catalog
        // does not know every model's, so fall back to a safe amount.
        let max_tokens = self.max_tokens.or_else(|| {
            model
                .reference()
                .starts_with("anthropic/")
                .then_some(32_000)
        });
        if let Some(max_tokens) = max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }

    /// A new answer's checkpoint in thread `id`, when checkpoints are on.
    fn turn(&self, id: &SessionId) -> Option<Arc<Turn>> {
        self.checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.turn(id))
    }

    /// The tool that starts subagents with the run's tools, sharing the
    /// thread's tool permissions, on the model the config gives them or the
    /// run's.
    fn spawn(
        &self,
        crew: &Arc<Crew>,
        id: &SessionId,
        thread: &Thread,
        request: &RunRequest,
        kit: Kit,
    ) -> Spawn {
        Spawn {
            crew: crew.clone(),
            team: crew.team(id, thread.permissions.clone()),
            kit,
            model: self
                .subagent_model
                .clone()
                .unwrap_or_else(|| request.options.model.clone()),
            thinking: request.options.thinking,
            kinds: subagents::kinds(&agent::working_directory()),
            hooks: Some(self.scope(id)),
        }
    }

    /// The config's hooks, run for thread `id`.
    fn scope(&self, id: &SessionId) -> Scope {
        Scope {
            hooks: self.hooks.clone(),
            conversation: id.as_str().to_owned(),
        }
    }

    /// Answers into `run` until it ends or is stopped, then learns from an
    /// answer once its clients are let go. The agent saves the thread as
    /// each of its streams ends; what a run that did not answer said, and
    /// the summary it compacted to, are saved here.
    async fn drive(
        self: Arc<Self>,
        id: SessionId,
        run: Arc<Run>,
        agent: Agent,
        answering: Answering,
    ) {
        let Answering {
            model,
            request,
            skills,
            live,
        } = answering;
        let notes = self.lock().get(&id).map(|thread| thread.notes.clone());
        let conversing = self.converse(&id, &run, &agent, &model, request, &skills, notes);
        let ending = until_stopped(&run, conversing)
            .await
            .unwrap_or(Ending::Interrupted);
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
            if let Some(Err(error)) =
                until_stopped(&run, self.compact_saved(&id, &run, live.compactor(), focus)).await
            {
                tracing::warn!(
                    thread = id.as_str(),
                    error = format!("{error:#}"),
                    "cannot compact"
                );
                run.emit(Event::Notice {
                    text: format!("not compacted: {error:#}"),
                });
            }
        }
        self.end(&id, &run, ending, context);
        if let Some(exchange) = exchange
            && let Err(error) = async { self.learn(&exchange?, &model).await }.await
        {
            tracing::warn!(
                thread = id.as_str(),
                error = format!("{error:#}"),
                "memory not updated"
            );
            *self.lock_memory_failure() = Some(format!("memory not updated: {error:#}"));
        }
        if answered {
            self.rename(&id, &model).await;
        }
    }

    /// Has a model name the thread from what it holds now, keeping the
    /// name it has unless the topic shifted enough.
    async fn rename(&self, id: &SessionId, model: &Model) {
        let result = async {
            let namer = self
                .compaction
                .model
                .as_ref()
                .map(|reference| Model::connect(reference, &self.providers))
                .transpose()?;
            let messages = self.sessions.messages(id)?;
            let current = self.sessions.names()?.remove(id);
            let naming = naming::name(
                namer.as_ref().unwrap_or(model),
                current.as_deref(),
                &messages,
            );
            let named = tokio::time::timeout(NAMING_TIMEOUT, naming)
                .await
                .context("naming took too long")??;
            match named {
                Some(name) if current.as_deref() != Some(&name) => {
                    self.sessions.set_name(id, &name)?;
                    anyhow::Ok(true)
                }
                _ => Ok(false),
            }
        }
        .await;
        match result {
            Ok(true) => self.publish(&self.lock()),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                thread = id.as_str(),
                error = format!("{error:#}"),
                "thread not named"
            ),
        }
    }

    /// Compacts the thread as saved into `run`, `focus` saying what the
    /// summary should keep above all.
    async fn drive_compaction(
        self: Arc<Self>,
        id: SessionId,
        run: Arc<Run>,
        compactor: Compactor,
        focus: String,
    ) {
        let ending =
            match until_stopped(&run, self.compact_saved(&id, &run, &compactor, focus)).await {
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
    async fn compact_saved(
        &self,
        id: &SessionId,
        run: &Run,
        compactor: &Compactor,
        focus: String,
    ) -> Result<()> {
        let basis = self.sessions.basis(id)?;
        run.emit(Event::Notice {
            text: "compacting the conversation…".into(),
        });
        let reason = CompactReason::Asked { focus };
        let Some(summary) = compactor
            .summarize(&basis.messages, basis.from, &basis.state, &reason)
            .await?
        else {
            run.emit(Event::Notice {
                text: "nothing to compact yet".into(),
            });
            return Ok(());
        };
        let kept = basis
            .messages
            .get(summary.upto)
            .context("nothing is kept after the summary")?;
        anyhow::ensure!(
            self.sessions
                .summarize(id, summary.upto, kept, &summary.state)?,
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
        let failure = match self
            .sessions
            .summarize(id, applied.at, &applied.kept, &applied.state)
        {
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
        match &ending {
            Ending::Failed { error } => {
                tracing::warn!(thread = id.as_str(), run = ?run.id(), error, "run failed")
            }
            Ending::Answered | Ending::Interrupted => {
                tracing::info!(thread = id.as_str(), run = ?run.id(), ?ending, "run ended")
            }
        }
        let saved = self
            .sessions
            .messages(id)
            .map_or(run.history(), |messages| messages.len());
        // Under the lock, so `open` sees either the run or all it saved,
        // and the thread is marked before any client hears it ended.
        let mut threads = self.lock();
        if let Err(error) = self.sessions.mark(id, |mark| mark.unseen = true) {
            tracing::warn!(
                thread = id.as_str(),
                error = format!("{error:#}"),
                "cannot mark as new"
            );
            run.emit(Event::Notice {
                text: format!("not marked as new: {error:#}"),
            });
        }
        run.emit(Event::End { ending, saved });
        for thread in threads.values_mut() {
            thread.ended.take_if(|ended| ended.at.elapsed() > RESUMABLE);
        }
        if let Some(thread) = threads.get_mut(id) {
            let ended = thread.run.take();
            thread.ended = ended.map(|run| Ended {
                run,
                at: Instant::now(),
            });
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
            Ending::Interrupted if self.closing.load(Ordering::SeqCst) => {
                "the host restarted; go on from here"
            }
            Ending::Interrupted => "the user stopped the reply",
            Ending::Failed { error } => error,
        };
        let said = run.record().said(reason);
        if said.is_empty() {
            return;
        }
        let count = said.len();
        match self.sessions.append(id, run.record().held(), said) {
            Ok(true) => tracing::info!(
                thread = id.as_str(),
                messages = count,
                "saved what an unfinished run said"
            ),
            Ok(false) => tracing::info!(
                thread = id.as_str(),
                "an unfinished run's thread changed; saved nothing"
            ),
            Err(error) => {
                tracing::warn!(
                    thread = id.as_str(),
                    error = format!("{error:#}"),
                    "cannot save an unfinished run"
                );
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
        notes: Option<Arc<Notes>>,
    ) -> Ending {
        let RunRequest {
            prompt,
            attachments,
            options,
            goal,
            ..
        } = request;
        let scope = self.scope(id);
        let mut added = Vec::new();
        if run.history() == 0 && scope.has(HookEvent::SessionStart) {
            let verdict = scope
                .fire(HookEvent::SessionStart, None, serde_json::json!({}))
                .await;
            verdict.tell(run);
            added.extend(verdict.context());
        }
        if scope.has(HookEvent::UserPromptSubmit) {
            let fields = serde_json::json!({ "prompt": prompt });
            let verdict = scope.fire(HookEvent::UserPromptSubmit, None, fields).await;
            verdict.tell(run);
            if let Some(reason) = verdict.block {
                return Ending::Failed {
                    error: format!("a hook blocked the message: {reason}"),
                };
            }
            added.extend(verdict.context());
        }
        let mut message = self
            .message(&prompt, &attachments, skills, model, run)
            .await;
        let planning = || notes.as_ref().is_some_and(|notes| notes.planning());
        if let Message::User { content } = &mut message {
            content.extend(added.into_iter().map(UserContent::text));
        }
        let (mut stops, mut rounds) = (0, 0);
        loop {
            if planning()
                && let Message::User { content } = &mut message
            {
                content.push(rig_core::message::UserContent::text(tools::PLAN_REMINDER));
            }
            let ending = self
                .respond(id, run, agent, message, options.thinking)
                .await;
            let waiting = match ending {
                Ending::Answered => run.last_steers(),
                Ending::Interrupted | Ending::Failed { .. } => Vec::new(),
            };
            if !waiting.is_empty() {
                for steering in &waiting {
                    run.emit(Event::Steered {
                        prompt: steering.prompt.clone(),
                    });
                }
                message = steer::message(waiting);
                continue;
            }
            if !matches!(ending, Ending::Answered) {
                return ending;
            }
            let reply = self
                .sessions
                .messages(id)
                .ok()
                .and_then(|messages| transcript::last_reply(&messages))
                .unwrap_or_default();
            let resumed = match self.stop_hooks(&scope, run, &reply, &mut stops).await {
                Some(reason) => Some(reason),
                None => {
                    self.pursue(run, model, goal.as_deref(), &reply, &mut rounds)
                        .await
                }
            };
            let Some(resumed) = resumed else {
                return ending;
            };
            // Taken again, as the run takes more messages while it goes on.
            run.reopen_steers();
            message = Message::user(resumed);
        }
    }

    /// Why a `Stop` hook sends the agent back to work after `reply`, at most
    /// [`MAX_STOP_BLOCKS`] times in a row.
    async fn stop_hooks(
        &self,
        scope: &Scope,
        run: &Run,
        reply: &str,
        stops: &mut usize,
    ) -> Option<String> {
        if !scope.has(HookEvent::Stop) {
            return None;
        }
        let fields = serde_json::json!({ "last_reply": reply, "stop_hook_active": *stops > 0 });
        let verdict = scope.fire(HookEvent::Stop, None, fields).await;
        verdict.tell(run);
        let reason = verdict.block?;
        if *stops >= MAX_STOP_BLOCKS {
            run.emit(Event::Notice {
                text: format!(
                    "a Stop hook blocked {MAX_STOP_BLOCKS} times in a row; stopping anyway"
                ),
            });
            return None;
        }
        *stops += 1;
        run.emit(Event::Notice {
            text: format!("a Stop hook sent the agent back to work: {reason}"),
        });
        Some(reason)
    }

    /// What the agent is told to go on with when `reply` does not meet
    /// `goal`, as the judge decides, for at most `[goal] max_rounds`.
    async fn pursue(
        &self,
        run: &Run,
        model: &Model,
        goal: Option<&str>,
        reply: &str,
        rounds: &mut usize,
    ) -> Option<String> {
        let goal = goal?;
        let reference = self.goal.model.as_ref().or(self.compaction.model.as_ref());
        let judge = match reference
            .map(|reference| Model::connect(reference, &self.providers))
            .transpose()
        {
            Ok(judge) => judge,
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "goal not judged");
                run.emit(Event::Notice {
                    text: format!("goal not judged: {error:#}"),
                });
                return None;
            }
        };
        run.emit(Event::Notice {
            text: "judging the goal…".into(),
        });
        let judged = until_stopped(
            run,
            goal::judge(judge.as_ref().unwrap_or(model), goal, reply),
        )
        .await?;
        match judged {
            Ok(judged) if judged.met => {
                run.emit(Event::Notice {
                    text: format!("goal met: {}", judged.reason)
                        .trim_end_matches([' ', ':'])
                        .to_owned(),
                });
                None
            }
            Ok(_) if *rounds >= self.goal.max_rounds => {
                run.emit(Event::Notice {
                    text: format!(
                        "goal not met after {} rounds; stopping",
                        self.goal.max_rounds
                    ),
                });
                None
            }
            Ok(judged) => {
                *rounds += 1;
                run.emit(Event::Notice {
                    text: format!(
                        "goal not met ({}/{}): {}",
                        rounds, self.goal.max_rounds, judged.reason
                    ),
                });
                Some(goal::prompt(goal, &judged.reason))
            }
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "goal not judged");
                run.emit(Event::Notice {
                    text: format!("goal not judged: {error:#}"),
                });
                None
            }
        }
    }

    /// Answers `message`, going on from where the answer got to while the
    /// model fails for a reason that may pass, as long as retries are left.
    async fn respond(
        &self,
        id: &SessionId,
        run: &Run,
        agent: &Agent,
        mut message: Message,
        thinking: Option<Thinking>,
    ) -> Ending {
        let mut attempts = Attempts::new(Backoff::MODEL);
        loop {
            let error = match self
                .attempt(id, run, agent, message.clone(), thinking, &mut attempts)
                .await
            {
                Ok(()) => return Ending::Answered,
                Err(error) => error,
            };
            let told = crate::log::chain(&error);
            let Some(retry) =
                retry::transient(&error).and_then(|transient| attempts.next(transient.asked))
            else {
                return Ending::Failed { error: told };
            };
            let Some(resumed) = self.resume(id, run, &told, message) else {
                return Ending::Failed { error: told };
            };
            message = resumed;
            tracing::warn!(thread = id.as_str(), run = ?run.id(), error = told, %retry, "model call failed");
            run.emit(Event::Notice {
                text: format!("{told}; {retry}"),
            });
            tokio::time::sleep(retry.delay).await;
        }
    }

    /// One try at answering `message`. The retries made start counting
    /// again once a tool call's result shows the answer got somewhere.
    async fn attempt(
        &self,
        id: &SessionId,
        run: &Run,
        agent: &Agent,
        message: Message,
        thinking: Option<Thinking>,
        attempts: &mut Attempts,
    ) -> Result<(), Box<PromptError>> {
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
        while let Some(item) = stream.next().await {
            let item = item.map_err(Box::new)?;
            if matches!(item, MultiTurnStreamItem::ToolResult { .. }) {
                attempts.reset();
            }
            steps
                .events(item)
                .into_iter()
                .for_each(|event| run.emit(event));
        }
        Ok(())
    }

    /// What to answer after the answer to `message` failed for `reason`:
    /// `message` again when nothing past it was said, else a request to go
    /// on, once what was said is saved; `None` when it could not be.
    fn resume(&self, id: &SessionId, run: &Run, reason: &str, message: Message) -> Option<Message> {
        let said = run.record().said(reason);
        if said.len() <= 1 {
            return Some(message);
        }
        let failure = match self.sessions.append(id, run.record().held(), said) {
            Ok(true) => return Some(Message::user(CONTINUE)),
            Ok(false) => "the conversation changed meanwhile".to_owned(),
            Err(error) => format!("{error:#}"),
        };
        tracing::warn!(
            thread = id.as_str(),
            failure,
            "cannot save a failed answer to go on from"
        );
        None
    }

    /// Learns from what a run said: the `exchange` its answer added. Typed
    /// extraction forces a tool call, which some providers (such as GitHub
    /// Copilot, for some models) reject outright; when the model tried
    /// first fails this way, retries once against this host's default
    /// model, which is expected to support it.
    async fn learn(&self, exchange: &[Message], model: &Model) -> Result<()> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let configured = memory
            .model()
            .map(|reference| Model::connect(reference, &self.providers))
            .transpose()?;
        let primary = configured.as_ref().unwrap_or(model);
        match memory.learn(primary, exchange).await {
            Ok(()) => Ok(()),
            Err(error) if primary.reference() != self.defaults.model => {
                tracing::warn!(
                    model = primary.reference(),
                    error = format!("{error:#}"),
                    "memory not updated with this model; falling back to the default model"
                );
                let fallback = Model::connect(&self.defaults.model, &self.providers)?;
                memory.learn(&fallback, exchange).await
            }
            Err(error) => Err(error),
        }
    }

    fn memory(&self) -> Result<&Memory> {
        self.memory
            .as_deref()
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
