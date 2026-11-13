/* -------------------------------------------------------------------------- */
/*                                protocol.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rig_core::message::{Message, ToolResult};
use serde::{Deserialize, Serialize};

use crate::config::Thinking;
use crate::permissions::Permission;
use crate::sessions::{SessionId, Summary};

/// What happens during a run, in order. Every client following a thread
/// draws the same events, whichever of them started the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first event of every run, and of every client's view of it.
    Began {
        run: RunId,
        /// How long the run had gone when the host sent this, so a client
        /// that joins late keeps the same clock as one that was there.
        #[serde(default)]
        elapsed: Duration,
    },
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// The model finished a part of its reply.
    PartEnd,
    /// The model finished a part of reasoning, which took `duration`.
    Thought {
        duration: Duration,
    },
    /// The model began writing a call to `name`; [`Event::ToolCall`]
    /// follows once the call is complete and runs.
    ToolCallStart {
        name: String,
    },
    /// More of the arguments of the call the model is writing: a piece of
    /// raw JSON.
    ToolCallArguments {
        json: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    /// A piece of a running command's output.
    Output {
        text: String,
    },
    ToolResult {
        result: ToolResult,
    },
    Notice {
        text: String,
    },
    /// A mention the host resolved attached this to the prompt.
    Attached {
        name: String,
    },
    /// The run waits until a client decides `ask`.
    Approval {
        id: ApprovalId,
        ask: Ask,
    },
    Decided {
        id: ApprovalId,
    },
    /// A subagent began a task, `label` saying what.
    AgentStarted {
        agent: AgentId,
        label: String,
        machine: String,
        /// As the parent asked for it; `None` is the machine's working
        /// directory.
        directory: Option<String>,
    },
    /// Something a subagent did, shown as part of its parent's run.
    Agent {
        agent: AgentId,
        step: Box<Event>,
    },
    AgentEnded {
        agent: AgentId,
        error: Option<String>,
    },
    /// A subagent's task waits for a client on `task.machine` to run it.
    Delegated {
        id: DelegationId,
        task: Task,
    },
    /// A client took up the delegated task.
    Claimed {
        id: DelegationId,
    },
    /// Tokens the last model call took in and gave back: how full the
    /// context is.
    Context {
        tokens: u64,
    },
    End {
        ending: Ending,
        /// How many messages the thread holds now, its answer saved.
        saved: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Ending {
    Answered,
    Interrupted,
    Failed { error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalId(pub u64);

/// What a client is asked to decide before the run goes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ask {
    /// Whether a tool call may run; `agent` is the label of the subagent
    /// making it.
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    /// Whether subagents may work in `path` on `machine`.
    Directory { machine: String, path: String },
}

/// A subagent, for its parent to follow up with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new() -> Self {
        Self(format!("{:06x}", getrandom::u32().unwrap_or_default() & 0xff_ffff))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DelegationId(pub u64);

impl fmt::Display for DelegationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A turn of a subagent's conversation, run on `machine`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub agent: AgentId,
    pub label: String,
    pub machine: String,
    /// Absolute, `~/`, or relative to the machine's working directory;
    /// `None` is the working directory itself.
    pub directory: Option<String>,
    pub message: String,
    /// Whether it continues the agent's earlier conversation.
    pub follow_up: bool,
    pub model: String,
    pub thinking: Option<Thinking>,
}

/// The subagent's final reply, or why it has none.
pub type TaskResult = Result<String, String>;

/// One run, distinct from every other on the host, even across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub u64);

/// What a thread is doing, as the activity stream tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// Its latest run since the host started; a client that sees this
    /// change knows a run happened, however briefly.
    pub run: Option<RunId>,
    pub status: ThreadStatus,
    #[serde(default)]
    pub archived: bool,
}

/// The threads that ran since the host started or have something new.
pub type Activities = BTreeMap<SessionId, Activity>;

impl fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Once,
    /// Allow the tool for the rest of the thread.
    Always,
    Decline,
    Stop,
}

/// What a run uses, chosen by the client; the host fills in the rest from
/// its own config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOptions {
    pub model: String,
    /// `None` leaves reasoning to the model's default.
    pub thinking: Option<Thinking>,
    /// `None` keeps the host's system prompt.
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub prompt: String,
    /// What the prompt's mentions attach from the user's working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(flatten)]
    pub options: RunOptions,
    /// The remote client asking; `None` when the host is in the asker's
    /// process, or the client is older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
}

/// A machine hey runs on, as the model is told of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub hostname: Option<String>,
    pub os: String,
    pub arch: String,
    pub cwd: Option<String>,
    /// The config file hey reads there, whether or not it exists.
    pub config: Option<String>,
}

impl Machine {
    pub fn here(config: Option<&Path>) -> Self {
        Self {
            hostname: gethostname::gethostname().into_string().ok(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cwd: std::env::current_dir()
                .ok()
                .map(|dir| dir.display().to_string()),
            config: config.map(|path| path.display().to_string()),
        }
    }
}

/// A remote client's machine, and the host as the client names it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub machine: Machine,
    pub remote: String,
    pub url: String,
    /// The machine name subagents on the client run under, when it runs
    /// them; its `[subagents.machines]` rules are keyed by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<String>,
}

/// A file or directory a mention attaches, as read where it was found, by
/// its absolute path there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    File {
        path: String,
        text: String,
    },
    Directory {
        path: String,
        /// Paths under it that git would not ignore, each directory's
        /// ending in `/`.
        entries: Vec<String>,
        /// Whether there were more than `entries`.
        truncated: bool,
    },
}

impl Attachment {
    /// How clients show it: its path, a directory's ending in `/`.
    pub fn name(&self) -> String {
        match self {
            Attachment::File { path, .. } => path.clone(),
            Attachment::Directory { path, .. } if path.ends_with('/') => path.clone(),
            Attachment::Directory { path, .. } => format!("{path}/"),
        }
    }
}

/// The host's configured model and thinking level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    pub model: String,
    pub thinking: Option<Thinking>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub reference: String,
    pub context_window: Option<u32>,
}

/// A thread as saved, and the run in progress on it, if any.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Messages up to the run in progress, which saves its own once it ends.
    pub messages: Vec<Message>,
    pub running: Option<Running>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Running {
    pub prompt: String,
}

/// A conversation as a listing shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadInfo {
    #[serde(flatten)]
    pub summary: Summary,
    pub status: ThreadStatus,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Nothing new since a client last showed it.
    Idle,
    /// An answer ended that no client has shown yet.
    Unseen,
    Running,
    /// The run waits for a client to approve something, or to take up a
    /// subagent's task.
    Asking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tools {
    pub permissions: BTreeMap<String, Permission>,
    pub servers: Vec<ServerReport>,
}

/// How an MCP server's connection went.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerReport {
    pub name: String,
    pub tools: Result<usize, String>,
}
