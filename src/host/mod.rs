/* -------------------------------------------------------------------------- */
/*                                host/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod run;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Result;
use futures::StreamExt;
use futures::stream::BoxStream;
use rig_agent::agent::{Agent, StreamingResult};
use rig_core::completion::GenerationOptions;
use rig_tools::{Live, Seen};

use crate::agent::{self, BUILTIN_TOOLS, Model};
use crate::config::{Config, Providers};
use crate::mcp;
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
    mcp: Vec<mcp::Tool>,
    servers: Vec<ServerReport>,
    permissions: Permissions,
    threads: Mutex<HashMap<SessionId, Thread>>,
}

/// What a thread keeps on the host between runs.
struct Thread {
    seen: Seen,
    permissions: Arc<Permissions>,
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
            mcp,
            servers,
            permissions,
            threads: Mutex::default(),
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
    pub fn start(
        self: &Arc<Self>,
        id: SessionId,
        request: RunRequest,
    ) -> Result<Events, StartError> {
        let RunRequest { prompt, options } = request;
        let model = Model::connect(&options.model, &self.providers).map_err(StartError::Refused)?;
        let mut threads = self.lock();
        let thread = threads.entry(id.clone()).or_insert_with(|| Thread {
            seen: Seen::default(),
            permissions: Arc::new(self.permissions.fork()),
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
        let run = Arc::new(Run::new(prompt.clone(), history));
        let agent = self.agent(thread, &model, &options, &run);
        let runner = agent.prompt(prompt).conversation(id.as_str());
        let stream = match options.thinking {
            Some(thinking) => runner.options(GenerationOptions::default().reasoning(thinking)),
            None => runner,
        }
        .stream();
        let events = run.events().boxed();
        thread.run = Some(run.clone());
        drop(threads);
        tokio::spawn(self.clone().drive(id, run, stream));
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
        if let Some(max_tokens) = self.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        builder.build()
    }

    /// Feeds the agent's stream into `run` until it ends or is stopped. The
    /// agent saves the thread as its stream ends.
    async fn drive(self: Arc<Self>, id: SessionId, run: Arc<Run>, mut stream: StreamingResult) {
        let ending = loop {
            let item = tokio::select! {
                item = stream.next() => item,
                () = run.stopped() => break Ending::Interrupted,
            };
            match item {
                None => break Ending::Answered,
                Some(Err(error)) => {
                    break Ending::Failed {
                        error: error.to_string(),
                    };
                }
                Some(Ok(item)) => {
                    if let Some(event) = run::event(item) {
                        run.emit(event);
                    }
                }
            }
        };
        drop(stream);
        // Under the lock, so `open` sees either the run or all it saved.
        let mut threads = self.lock();
        run.emit(Event::End { ending });
        if let Some(thread) = threads.get_mut(&id) {
            thread.run = None;
        }
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
