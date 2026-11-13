/* -------------------------------------------------------------------------- */
/*                           subagents/delegated.rs                           */
/* -------------------------------------------------------------------------- */

use std::sync::Arc;

use super::Crew;
use crate::client::Client;
use crate::permissions::Reporter;
use crate::protocol::{Ask, Decision, DelegationId, Event, Task};
use crate::sessions::SessionId;

/// Runs a task the remote host delegated to this machine, unless another
/// client takes it up first. Everything it does is shown, and everything it
/// asks is decided, through the host's run, as for a subagent on the host.
pub async fn take_up(client: Client, crew: Arc<Crew>, thread: SessionId, id: DelegationId, task: Task) {
    if !client.claim(&thread, id).await.unwrap_or(false) {
        return;
    }
    let team = crew.team(&thread, None);
    let reporter = Relay {
        client: client.clone(),
        thread: thread.clone(),
        id,
    };
    let result = crew.run(&team, &task, reporter).await;
    // If the host cannot hear it, its run waits until a client stops it.
    _ = client.finish(&thread, id, &result).await;
}

/// Reports a delegated subagent's events and questions to the host's run.
struct Relay {
    client: Client,
    thread: SessionId,
    id: DelegationId,
}

impl Reporter for Relay {
    async fn emit(&self, event: Event) {
        _ = self.client.delegated_event(&self.thread, self.id, &event).await;
    }

    /// A question the host cannot take, as when its run ended, stops the agent.
    async fn ask(&self, ask: Ask) -> Decision {
        self.client
            .delegated_ask(&self.thread, self.id, &ask)
            .await
            .unwrap_or(Decision::Stop)
    }

    async fn stop(&self) {
        _ = self.client.stop(&self.thread).await;
    }
}
