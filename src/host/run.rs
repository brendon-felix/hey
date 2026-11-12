/* -------------------------------------------------------------------------- */
/*                                host/run.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use futures::{Stream, StreamExt, stream};
use rig_agent::agent::MultiTurnStreamItem;
use rig_core::completion::Usage;
use rig_core::streaming::{Item, StreamEvent};
use tokio::sync::{Notify, oneshot, watch};

use crate::protocol::{ApprovalId, Decision, Event, Running};

/// A run in progress on a thread. The host owns it, not a client: clients
/// come and go, replaying and following its events and answering its
/// approvals, while it carries on.
pub struct Run {
    prompt: String,
    /// How many messages the thread held when the run began.
    history: usize,
    log: watch::Sender<Vec<Event>>,
    approvals: Mutex<HashMap<ApprovalId, oneshot::Sender<Decision>>>,
    next_approval: AtomicU64,
    stop: Notify,
}

impl Run {
    pub fn new(prompt: String, history: usize) -> Self {
        Self {
            prompt,
            history,
            log: watch::Sender::new(Vec::new()),
            approvals: Mutex::default(),
            next_approval: AtomicU64::new(0),
            stop: Notify::new(),
        }
    }

    pub fn running(&self) -> Running {
        Running {
            prompt: self.prompt.clone(),
        }
    }

    pub fn history(&self) -> usize {
        self.history
    }

    pub fn emit(&self, event: Event) {
        self.log.send_modify(|log| log.push(event));
    }

    /// Waits for a client to decide whether `tool` may run.
    pub async fn ask(&self, tool: &str) -> Decision {
        let id = ApprovalId(self.next_approval.fetch_add(1, Ordering::Relaxed));
        let (decide, decision) = oneshot::channel();
        self.lock_approvals().insert(id, decide);
        self.emit(Event::Approval {
            id,
            tool: tool.to_owned(),
        });
        decision.await.unwrap_or(Decision::Stop)
    }

    /// False when `id` is not waiting, as when another client decided first.
    pub fn answer(&self, id: ApprovalId, decision: Decision) -> bool {
        let Some(decide) = self.lock_approvals().remove(&id) else {
            return false;
        };
        self.emit(Event::Decided { id });
        decide.send(decision).is_ok()
    }

    pub fn stop(&self) {
        self.stop.notify_one();
    }

    pub async fn stopped(&self) {
        self.stop.notified().await;
    }

    /// The events so far, then each as it happens, until the run ends.
    pub fn events(&self) -> impl Stream<Item = Event> + Send + 'static {
        let log = self.log.subscribe();
        stream::unfold(Some((log, 0)), |state| async move {
            let (mut log, mut read) = state?;
            loop {
                let (batch, len) = {
                    let events = log.borrow_and_update();
                    (
                        undecided(events.get(read..).unwrap_or_default()),
                        events.len(),
                    )
                };
                read = len;
                if !batch.is_empty() {
                    let ended = matches!(batch.last(), Some(Event::End { .. }));
                    return Some((batch, (!ended).then_some((log, read))));
                }
                log.changed().await.ok()?;
            }
        })
        .flat_map(stream::iter)
    }

    fn lock_approvals(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ApprovalId, oneshot::Sender<Decision>>> {
        self.approvals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// `events` without the approvals they also show decided, so a client that
/// joins late is not asked about them.
fn undecided(events: &[Event]) -> Vec<Event> {
    let decided = |id: &ApprovalId| {
        events
            .iter()
            .any(|event| matches!(event, Event::Decided { id: other } if other == id))
    };
    events
        .iter()
        .filter(|event| !matches!(event, Event::Approval { id, .. } if decided(id)))
        .cloned()
        .collect()
}

/// What a client needs to see of a step of the agent's stream. Tool calls
/// are reported by the approval hook as they are dispatched.
pub fn event(item: MultiTurnStreamItem) -> Option<Event> {
    match item {
        MultiTurnStreamItem::StreamAssistantItem(Item::Event(event)) => match event {
            StreamEvent::Text { text, .. } => Some(Event::Text { text }),
            StreamEvent::Reasoning { text, .. } => Some(Event::Reasoning { text }),
            StreamEvent::End { .. } => Some(Event::PartEnd),
            _ => None,
        },
        MultiTurnStreamItem::ToolResult { tool_result } => Some(Event::ToolResult {
            result: tool_result,
        }),
        MultiTurnStreamItem::CompletionCall(call) => {
            tokens(&call.usage).map(|tokens| Event::Context { tokens })
        }
        MultiTurnStreamItem::ModelTurnRetried { .. } => Some(Event::Notice {
            text: "retrying…".into(),
        }),
        _ => None,
    }
}

fn tokens(usage: &Usage) -> Option<u64> {
    usage
        .total_tokens
        .or(match (usage.input_tokens, usage.output_tokens) {
            (None, None) => None,
            (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
        })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::protocol::Ending;

    fn names(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                Event::Text { text } => text.clone(),
                Event::Approval { tool, .. } => format!("ask {tool}"),
                Event::Decided { .. } => "decided".into(),
                Event::End { .. } => "end".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn late_clients_replay_the_run_without_decided_approvals() {
        let run = std::sync::Arc::new(Run::new("hi".into(), 0));
        let mut early = Box::pin(run.events());
        run.emit(Event::Text { text: "a".into() });
        let asking = tokio::spawn({
            let run = run.clone();
            async move { run.ask("shell").await }
        });
        let mut asked = Vec::new();
        while asked.len() < 2 {
            asked.extend(early.next().await);
        }
        assert_eq!(names(&asked), ["a", "ask shell"]);
        let joined: Vec<Event> = run.events().take(2).collect().await;
        assert_eq!(
            names(&joined),
            ["a", "ask shell"],
            "a pending approval replays"
        );

        assert!(run.answer(ApprovalId(0), Decision::Once));
        assert!(
            !run.answer(ApprovalId(0), Decision::Decline),
            "decided only once"
        );
        assert_eq!(asking.await.unwrap(), Decision::Once);
        run.emit(Event::End {
            ending: Ending::Answered,
        });

        let rest: Vec<Event> = early.collect().await;
        assert_eq!(names(&rest), ["decided", "end"]);
        let late: Vec<Event> = run.events().collect().await;
        assert_eq!(names(&late), ["a", "decided", "end"]);
    }
}
