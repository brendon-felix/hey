/* -------------------------------------------------------------------------- */
/*                                 backend.rs                                 */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::client::Client;
use crate::host::{self, Host};
use crate::protocol::{
    ApprovalId, Decision, Defaults, Event, ModelInfo, RunRequest, Snapshot, ThreadInfo, Tools,
};
use crate::sessions::SessionId;

pub type Events = BoxStream<'static, Result<Event>>;

/// A thread as saved, and the events of its run in progress.
pub struct Opened {
    pub snapshot: Snapshot,
    pub events: Option<Events>,
}

/// Where the agent runs and threads are kept: in this process, or on a
/// remote host.
pub enum Backend {
    Local(Arc<Host>),
    Remote(Client),
}

impl Backend {
    /// The remote's name, for the status line.
    pub fn remote(&self) -> Option<&str> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(client) => Some(client.name()),
        }
    }

    pub async fn defaults(&self) -> Result<Defaults> {
        match self {
            Backend::Local(host) => Ok(host.defaults()),
            Backend::Remote(client) => client.defaults().await,
        }
    }

    pub async fn model(&self, reference: &str) -> Result<ModelInfo> {
        match self {
            Backend::Local(host) => host.model(reference),
            Backend::Remote(client) => client.model(reference).await,
        }
    }

    /// Models with tool support whose vendor has an API key on the host.
    pub async fn models(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => Ok(host.models()),
            Backend::Remote(client) => client.models().await,
        }
    }

    /// Every thread with its status, newest first.
    pub async fn threads(&self) -> Result<Vec<ThreadInfo>> {
        match self {
            Backend::Local(host) => host.threads(),
            Backend::Remote(client) => client.threads().await,
        }
    }

    /// Tells the host the thread was shown up to its latest answer.
    pub async fn viewed(&self, id: &SessionId) -> Result<()> {
        match self {
            Backend::Local(host) => host.viewed(id),
            Backend::Remote(client) => client.viewed(id).await,
        }
    }

    pub async fn archive(&self, id: &SessionId, archived: bool) -> Result<()> {
        match self {
            Backend::Local(host) => host.archive(id, archived),
            Backend::Remote(client) => client.archive(id, archived).await,
        }
    }

    /// A new thread holding a copy of what `id` saved.
    pub async fn fork(&self, id: &SessionId) -> Result<SessionId> {
        match self {
            Backend::Local(host) => host.fork(id),
            Backend::Remote(client) => client.fork(id).await,
        }
    }

    /// Removes the thread, unless it is answering.
    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        match self {
            Backend::Local(host) => Ok(host.delete(id)?),
            Backend::Remote(client) => client.delete(id).await,
        }
    }

    /// Every path of the host's shared repository.
    pub async fn shared(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => Ok(host.shared()),
            Backend::Remote(client) => client.shared().await,
        }
    }

    pub async fn open(&self, id: &SessionId) -> Result<Opened> {
        match self {
            Backend::Local(host) => {
                let host::Opened { snapshot, events } = host.open(id)?;
                Ok(Opened {
                    snapshot,
                    events: events.map(|events| events.map(Ok).boxed()),
                })
            }
            Backend::Remote(client) => client.open(id).await,
        }
    }

    pub async fn start(&self, id: &SessionId, request: RunRequest) -> Result<Events> {
        match self {
            Backend::Local(host) => Ok(host.start(id.clone(), request).await?.map(Ok).boxed()),
            Backend::Remote(client) => client.start(id, &request).await,
        }
    }

    /// False when the approval was no longer waiting.
    pub async fn answer(
        &self,
        id: &SessionId,
        approval: ApprovalId,
        decision: Decision,
    ) -> Result<bool> {
        match self {
            Backend::Local(host) => Ok(host.answer(id, approval, decision)),
            Backend::Remote(client) => client.answer(id, approval, decision).await,
        }
    }

    /// False when the thread was not answering.
    pub async fn stop(&self, id: &SessionId) -> Result<bool> {
        match self {
            Backend::Local(host) => Ok(host.stop(id)),
            Backend::Remote(client) => client.stop(id).await,
        }
    }

    pub async fn tools(&self, id: &SessionId) -> Result<Tools> {
        match self {
            Backend::Local(host) => Ok(host.tools(id)),
            Backend::Remote(client) => client.tools(id).await,
        }
    }

    /// Everything the host remembers, oldest first.
    pub async fn memories(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => host.memories(),
            Backend::Remote(client) => client.memories().await,
        }
    }

    /// False when `memory` was not saved.
    pub async fn forget(&self, memory: &str) -> Result<bool> {
        match self {
            Backend::Local(host) => host.forget(memory),
            Backend::Remote(client) => client.forget(memory).await,
        }
    }

    /// Lets a host in this process finish learning from its answers before
    /// the process exits, unless Ctrl+C says not to wait. A remote host
    /// carries on by itself.
    pub async fn settle(&self) {
        if let Backend::Local(host) = self {
            tokio::select! {
                () = host.settle() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
    }
}
