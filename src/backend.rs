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
    Activities, ApprovalId, Decision, Defaults, DelegationId, Event, ModelInfo, Revision,
    RunRequest, Snapshot, Task, ThreadInfo, Tools,
};
use crate::sessions::SessionId;
use crate::skills::Found;
use crate::subagents::{self, Crew};

pub type Events = BoxStream<'static, Result<Event>>;

/// What the host's threads are doing, then each change, and a repeat at
/// least every `host::HEARTBEAT` while the host is reachable.
pub type ActivityStream = BoxStream<'static, Result<Activities>>;

/// A thread as saved, and the events of its run in progress.
pub struct Opened {
    pub snapshot: Snapshot,
    pub events: Option<Events>,
}

/// Where the agent runs and threads are kept: in this process, or on a
/// remote host, for which this machine may run subagents too.
#[derive(Clone)]
pub enum Backend {
    Local(Arc<Host>),
    Remote(Client, Option<Arc<Crew>>),
}

impl Backend {
    /// The remote's name, for the status line.
    pub fn remote(&self) -> Option<&str> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(client, _) => Some(client.name()),
        }
    }

    pub async fn defaults(&self) -> Result<Defaults> {
        match self {
            Backend::Local(host) => Ok(host.defaults()),
            Backend::Remote(client, _) => client.defaults().await,
        }
    }

    pub async fn model(&self, reference: &str) -> Result<ModelInfo> {
        match self {
            Backend::Local(host) => host.model(reference),
            Backend::Remote(client, _) => client.model(reference).await,
        }
    }

    /// Models with tool support whose vendor has an API key on the host.
    pub async fn models(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => Ok(host.models()),
            Backend::Remote(client, _) => client.models().await,
        }
    }

    /// Every thread with its status, newest first.
    pub async fn threads(&self) -> Result<Vec<ThreadInfo>> {
        match self {
            Backend::Local(host) => host.threads(),
            Backend::Remote(client, _) => client.threads().await,
        }
    }

    /// Tells the host the thread was shown up to its latest answer.
    pub async fn viewed(&self, id: &SessionId) -> Result<()> {
        match self {
            Backend::Local(host) => host.viewed(id),
            Backend::Remote(client, _) => client.viewed(id).await,
        }
    }

    pub async fn archive(&self, id: &SessionId, archived: bool) -> Result<()> {
        match self {
            Backend::Local(host) => host.archive(id, archived),
            Backend::Remote(client, _) => client.archive(id, archived).await,
        }
    }

    /// A new thread holding a copy of what `id` saved.
    pub async fn fork(&self, id: &SessionId) -> Result<SessionId> {
        match self {
            Backend::Local(host) => host.fork(id),
            Backend::Remote(client, _) => client.fork(id).await,
        }
    }

    /// Removes the thread, unless it is answering.
    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        match self {
            Backend::Local(host) => Ok(host.delete(id)?),
            Backend::Remote(client, _) => client.delete(id).await,
        }
    }

    /// What the host's threads are doing, then each change.
    pub async fn activity(&self) -> Result<ActivityStream> {
        match self {
            Backend::Local(host) => Ok(host.activity().map(Ok).boxed()),
            Backend::Remote(client, _) => client.activity().await,
        }
    }

    /// Every path of the host's shared repository.
    pub async fn shared(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => Ok(host.shared()),
            Backend::Remote(client, _) => client.shared().await,
        }
    }

    /// The skills the host's agent has, and why any were not loaded.
    pub async fn skills(&self) -> Result<Found> {
        match self {
            Backend::Local(host) => Ok(host.skills()),
            Backend::Remote(client, _) => client.skills().await,
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
            Backend::Remote(client, _) => client.open(id).await,
        }
    }

    pub async fn start(&self, id: &SessionId, request: RunRequest) -> Result<Events> {
        match self {
            Backend::Local(host) => Ok(host.start(id.clone(), request).await?.map(Ok).boxed()),
            Backend::Remote(client, _) => client.start(id, request).await,
        }
    }

    /// Runs a subagent's task delegated to this machine, in the background,
    /// unless it is for another machine or another client takes it up first.
    pub fn take_up(&self, thread: &SessionId, id: DelegationId, task: Task) -> Option<impl Future<Output = ()> + Send + 'static> {
        match self {
            Backend::Remote(client, Some(crew)) if task.machine == crew.machine() => Some(
                subagents::take_up(client.clone(), crew.clone(), thread.clone(), id, task),
            ),
            _ => None,
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
            Backend::Remote(client, _) => client.answer(id, approval, decision).await,
        }
    }

    /// False when the thread was not answering.
    pub async fn stop(&self, id: &SessionId) -> Result<bool> {
        match self {
            Backend::Local(host) => Ok(host.stop(id)),
            Backend::Remote(client, _) => client.stop(id).await,
        }
    }

    pub async fn tools(&self, id: &SessionId) -> Result<Tools> {
        match self {
            Backend::Local(host) => Ok(host.tools(id)),
            Backend::Remote(client, _) => client.tools(id).await,
        }
    }

    /// Everything the host remembers, oldest first.
    pub async fn memories(&self) -> Result<Vec<String>> {
        match self {
            Backend::Local(host) => host.memories(),
            Backend::Remote(client, _) => client.memories().await,
        }
    }

    /// False when `memory` was not saved.
    pub async fn forget(&self, memory: &str) -> Result<bool> {
        match self {
            Backend::Local(host) => host.forget(memory),
            Backend::Remote(client, _) => client.forget(memory).await,
        }
    }

    /// False when it is already saved.
    pub async fn remember(&self, memory: &str) -> Result<bool> {
        match self {
            Backend::Local(host) => host.remember(memory),
            Backend::Remote(client, _) => client.remember(memory).await,
        }
    }

    /// Rewords a memory, or saves the new words when it is gone.
    pub async fn revise(&self, revision: &Revision) -> Result<()> {
        match self {
            Backend::Local(host) => host.revise(revision),
            Backend::Remote(client, _) => client.revise(revision).await,
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
