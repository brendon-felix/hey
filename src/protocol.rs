/* -------------------------------------------------------------------------- */
/*                                protocol.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::fmt;

use rig_core::message::{Message, ToolResult};
use serde::{Deserialize, Serialize};

use crate::config::Thinking;
use crate::permissions::Permission;

/// What happens during a run, in order. Every client following a thread
/// draws the same events, whichever of them started the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// The model finished a part of its reply.
    PartEnd,
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
    /// A tool call waits until a client decides it.
    Approval {
        id: ApprovalId,
        tool: String,
    },
    Decided {
        id: ApprovalId,
    },
    /// Tokens the last model call took in and gave back: how full the
    /// context is.
    Context {
        tokens: u64,
    },
    End {
        ending: Ending,
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
