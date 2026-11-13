/* -------------------------------------------------------------------------- */
/*                                host/run.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use futures::{Stream, StreamExt, stream};
use rig_agent::agent::MultiTurnStreamItem;
use rig_core::completion::Usage;
use rig_core::message::AssistantContent;
use rig_core::streaming::{Item, PartKind, StreamEvent};
use tokio::sync::{Notify, oneshot, watch};

use crate::protocol::{ApprovalId, Decision, Event, RunId, Running};

/// A run in progress on a thread. The host owns it, not a client: clients
/// come and go, replaying and following its events and answering its
/// approvals, while it carries on.
pub struct Run {
    id: RunId,
    started: Instant,
    prompt: String,
    /// How many messages the thread held when the run began.
    history: usize,
    log: watch::Sender<Vec<Event>>,
    approvals: Mutex<HashMap<ApprovalId, oneshot::Sender<Decision>>>,
    next_approval: AtomicU64,
    stop: Notify,
    /// Called when whether a tool call waits for a decision changes.
    asking_changed: Box<dyn Fn() + Send + Sync>,
}

impl Run {
    pub fn new(prompt: String, history: usize, asking_changed: impl Fn() + Send + Sync + 'static) -> Self {
        // Random rather than counted, so a restarted host cannot reuse the
        // id of a run a client has already shown.
        let id = RunId(getrandom::u64().unwrap_or_else(|_| {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
            now.map_or(0, |since| since.as_nanos() as u64)
        }));
        Self {
            id,
            started: Instant::now(),
            prompt,
            history,
            log: watch::Sender::default(),
            approvals: Mutex::default(),
            next_approval: AtomicU64::new(0),
            stop: Notify::new(),
            asking_changed: Box::new(asking_changed),
        }
    }

    pub fn id(&self) -> RunId {
        self.id
    }

    pub fn running(&self) -> Running {
        Running {
            prompt: self.prompt.clone(),
        }
    }

    pub fn history(&self) -> usize {
        self.history
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Whether a tool call waits for a client to decide it.
    pub fn asking(&self) -> bool {
        !self.lock_approvals().is_empty()
    }

    pub fn emit(&self, event: Event) {
        self.log.send_modify(|log| log.push(event));
    }

    /// Waits for a client to decide whether `tool` may run.
    pub async fn ask(&self, tool: &str) -> Decision {
        let id = ApprovalId(self.next_approval.fetch_add(1, Ordering::Relaxed));
        let (decide, decision) = oneshot::channel();
        self.lock_approvals().insert(id, decide);
        (self.asking_changed)();
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
        (self.asking_changed)();
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
        let (run, started) = (self.id, self.started);
        let began = stream::once(async move {
            Event::Began {
                run,
                elapsed: started.elapsed(),
            }
        });
        let log = self.log.subscribe();
        let logged = stream::unfold(Some((log, 0)), |state| async move {
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
        .flat_map(stream::iter);
        began.chain(logged)
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

/// What a client needs to see of the agent's stream, timing the model's
/// reasoning as it goes. Tool calls are reported by the approval hook as
/// they are dispatched.
#[derive(Default)]
pub struct Steps {
    /// When the reasoning part now open began.
    reasoning: Option<Instant>,
}

impl Steps {
    pub fn events(&mut self, item: MultiTurnStreamItem) -> impl Iterator<Item = Event> + use<> {
        let (thought, event) = match item {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(event)) => {
                (self.thought_before(&event), self.part(event))
            }
            MultiTurnStreamItem::ToolResult { tool_result } => (
                None,
                Some(Event::ToolResult {
                    result: tool_result,
                }),
            ),
            MultiTurnStreamItem::CompletionCall(call) => {
                (None, tokens(&call.usage).map(|tokens| Event::Context { tokens }))
            }
            MultiTurnStreamItem::ModelTurnRetried { .. } => (
                None,
                Some(Event::Notice {
                    text: "retrying…".into(),
                }),
            ),
            _ => (None, None),
        };
        thought.into_iter().chain(event)
    }

    /// Reasoning ends where the reply moves on, however much later a
    /// provider closes its part.
    fn thought_before(&mut self, event: &StreamEvent) -> Option<Event> {
        let moves_on = match event {
            StreamEvent::Start { kind, .. } => *kind != PartKind::Reasoning,
            StreamEvent::Text { .. } | StreamEvent::Arguments { .. } => true,
            StreamEvent::Reasoning { .. } | StreamEvent::End { .. } => false,
        };
        moves_on.then(|| self.thought()).flatten()
    }

    fn thought(&mut self) -> Option<Event> {
        let since = self.reasoning.take()?;
        Some(Event::Thought {
            duration: since.elapsed(),
        })
    }

    fn part(&mut self, event: StreamEvent) -> Option<Event> {
        match event {
            StreamEvent::Start {
                kind: PartKind::Reasoning,
                ..
            } => {
                self.reasoning.get_or_insert_with(Instant::now);
                None
            }
            StreamEvent::Start {
                kind: PartKind::ToolCall,
                name: Some(name),
                ..
            } => Some(Event::ToolCallStart { name: name.into() }),
            StreamEvent::Arguments { json, .. } => Some(Event::ToolCallArguments { json }),
            StreamEvent::Text { text, .. } => Some(Event::Text { text }),
            StreamEvent::Reasoning { text, .. } => Some(Event::Reasoning { text }),
            StreamEvent::End {
                content: AssistantContent::Reasoning(_),
                ..
            } => self.thought(),
            StreamEvent::End { .. } => Some(Event::PartEnd),
            _ => None,
        }
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
                Event::Began { .. } => "began".into(),
                Event::Text { text } => text.clone(),
                Event::Approval { tool, .. } => format!("ask {tool}"),
                Event::Decided { .. } => "decided".into(),
                Event::End { .. } => "end".into(),
                Event::Thought { .. } => "thought".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn late_clients_replay_the_run_without_decided_approvals() {
        let run = std::sync::Arc::new(Run::new("hi".into(), 0, || {}));
        let mut early = Box::pin(run.events());
        run.emit(Event::Text { text: "a".into() });
        let asking = tokio::spawn({
            let run = run.clone();
            async move { run.ask("shell").await }
        });
        let mut asked = Vec::new();
        while asked.len() < 3 {
            asked.extend(early.next().await);
        }
        assert_eq!(names(&asked), ["began", "a", "ask shell"]);
        assert!(matches!(asked[0], Event::Began { run: id, .. } if id == run.id()));
        let joined: Vec<Event> = run.events().take(3).collect().await;
        assert_eq!(
            names(&joined),
            ["began", "a", "ask shell"],
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
            saved: 2,
        });

        let rest: Vec<Event> = early.collect().await;
        assert_eq!(names(&rest), ["decided", "end"]);
        let late: Vec<Event> = run.events().collect().await;
        assert_eq!(names(&late), ["began", "a", "decided", "end"]);
    }

    #[tokio::test]
    async fn every_client_times_the_run_from_when_it_began() {
        let run = Run::new("hi".into(), 0, || {});
        std::thread::sleep(std::time::Duration::from_millis(20));
        let joined = Box::pin(run.events()).next().await;
        assert!(
            matches!(joined, Some(Event::Began { elapsed, .. }) if elapsed.as_millis() >= 20),
            "{joined:?}"
        );
    }

    #[test]
    fn reasoning_ends_where_the_reply_moves_on() {
        use rig_core::message::Reasoning;
        use rig_core::streaming::Part;

        let (thinking, answer) = (Part::new(0), Part::new(1));
        let stream = [
            StreamEvent::Start { part: thinking, kind: PartKind::Reasoning, name: None },
            StreamEvent::Reasoning { part: thinking, text: "hm".into() },
            StreamEvent::Start { part: answer, kind: PartKind::Text, name: None },
            StreamEvent::Text { part: answer, text: "hi".into() },
            StreamEvent::End { part: thinking, content: AssistantContent::Reasoning(Reasoning::new("hm")) },
            StreamEvent::End { part: answer, content: AssistantContent::text("hi") },
        ];
        let mut steps = Steps::default();
        let events: Vec<Event> = stream
            .into_iter()
            .flat_map(|event| steps.events(MultiTurnStreamItem::StreamAssistantItem(Item::Event(event))))
            .collect();
        assert_eq!(names(&events), ["Reasoning { text: \"hm\" }", "thought", "hi", "PartEnd"]);
    }
}
