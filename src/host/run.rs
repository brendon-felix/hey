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

use crate::protocol::{
    AgentId, ApprovalId, Ask, Decision, DelegationId, Event, RunId, Running, Task, TaskResult,
};

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
    delegations: Mutex<HashMap<DelegationId, Delegation>>,
    next_id: AtomicU64,
    stop: Notify,
    /// Called when whether the run waits for a client changes.
    asking_changed: Box<dyn Fn() + Send + Sync>,
}

/// A subagent's task that a client runs on its own machine.
struct Delegation {
    agent: AgentId,
    label: String,
    /// Whether a client took it up; until one does, the run waits for one.
    claimed: bool,
    done: oneshot::Sender<TaskResult>,
}

/// The agent and label of a claimed delegation, which a client's reports
/// about it are shown under.
pub struct Claimed {
    pub agent: AgentId,
    pub label: String,
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
            delegations: Mutex::default(),
            next_id: AtomicU64::new(0),
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

    /// Whether the run waits for a client: to decide something, or to take
    /// up a subagent's task.
    pub fn asking(&self) -> bool {
        !lock(&self.approvals).is_empty()
            || lock(&self.delegations).values().any(|delegation| !delegation.claimed)
    }

    pub fn emit(&self, event: Event) {
        self.log.send_modify(|log| log.push(event));
    }

    /// Waits for a client to decide `ask`.
    pub async fn ask(&self, ask: Ask) -> Decision {
        let id = ApprovalId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (decide, decision) = oneshot::channel();
        lock(&self.approvals).insert(id, decide);
        (self.asking_changed)();
        self.emit(Event::Approval { id, ask });
        decision.await.unwrap_or(Decision::Stop)
    }

    /// False when `id` is not waiting, as when another client decided first.
    pub fn answer(&self, id: ApprovalId, decision: Decision) -> bool {
        let Some(decide) = lock(&self.approvals).remove(&id) else {
            return false;
        };
        (self.asking_changed)();
        self.emit(Event::Decided { id });
        decide.send(decision).is_ok()
    }

    /// Waits for a client on `task.machine` to run `task` and report how it
    /// went. Dropped, as when the run stops, it is withdrawn.
    pub async fn delegate(&self, task: Task) -> TaskResult {
        let id = DelegationId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (done, result) = oneshot::channel();
        let delegation = Delegation {
            agent: task.agent.clone(),
            label: task.label.clone(),
            claimed: false,
            done,
        };
        lock(&self.delegations).insert(id, delegation);
        let _withdraw = Withdraw { run: self, id };
        (self.asking_changed)();
        self.emit(Event::Delegated { id, task });
        result
            .await
            .unwrap_or_else(|_| Err("the client running it went away".into()))
    }

    /// Takes up delegation `id` for a client; false when another client took
    /// it first or it is no longer waiting.
    pub fn claim(&self, id: DelegationId) -> bool {
        let claimed = match lock(&self.delegations).get_mut(&id) {
            Some(delegation) if !delegation.claimed => {
                delegation.claimed = true;
                true
            }
            _ => false,
        };
        if claimed {
            (self.asking_changed)();
            self.emit(Event::Claimed { id });
        }
        claimed
    }

    /// The subagent of a claimed delegation, while it is still waited for.
    pub fn claimed(&self, id: DelegationId) -> Option<Claimed> {
        lock(&self.delegations)
            .get(&id)
            .filter(|delegation| delegation.claimed)
            .map(|delegation| Claimed {
                agent: delegation.agent.clone(),
                label: delegation.label.clone(),
            })
    }

    /// Ends a claimed delegation with `result`; false when it was not waited for.
    pub fn finish(&self, id: DelegationId, result: TaskResult) -> bool {
        let mut delegations = lock(&self.delegations);
        if !delegations.get(&id).is_some_and(|delegation| delegation.claimed) {
            return false;
        }
        delegations
            .remove(&id)
            .is_some_and(|delegation| delegation.done.send(result).is_ok())
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
}

/// Removes a delegation once nothing waits for it.
struct Withdraw<'a> {
    run: &'a Run,
    id: DelegationId,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        let withdrawn = lock(&self.run.delegations).remove(&self.id);
        if withdrawn.is_some_and(|delegation| !delegation.claimed) {
            (self.run.asking_changed)();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `events` without the approvals they also show decided and the
/// delegations they show claimed, so a client that joins late is not asked
/// about them.
fn undecided(events: &[Event]) -> Vec<Event> {
    let decided = |id: &ApprovalId| {
        events
            .iter()
            .any(|event| matches!(event, Event::Decided { id: other } if other == id))
    };
    let claimed = |id: &DelegationId| {
        events
            .iter()
            .any(|event| matches!(event, Event::Claimed { id: other } if other == id))
    };
    events
        .iter()
        .filter(|event| match event {
            Event::Approval { id, .. } => !decided(id),
            Event::Delegated { id, .. } => !claimed(id),
            _ => true,
        })
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
                Event::Approval {
                    ask: Ask::Tool { name, .. },
                    ..
                } => format!("ask {name}"),
                Event::Delegated { task, .. } => format!("delegate {}", task.label),
                Event::Claimed { .. } => "claimed".into(),
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
            async move {
                let ask = Ask::Tool {
                    name: "shell".into(),
                    agent: None,
                };
                run.ask(ask).await
            }
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

    #[tokio::test]
    async fn a_delegation_waits_for_one_client_to_claim_and_finish_it() {
        let run = std::sync::Arc::new(Run::new("hi".into(), 0, || {}));
        let task = Task {
            agent: AgentId("a1".into()),
            label: "survey".into(),
            machine: "laptop".into(),
            directory: None,
            message: "look around".into(),
            follow_up: false,
            model: "openai/gpt-5.5".into(),
            thinking: None,
        };
        let delegated = tokio::spawn({
            let run = run.clone();
            async move { run.delegate(task).await }
        });
        let mut events = Box::pin(run.events());
        let id = loop {
            if let Some(Event::Delegated { id, .. }) = events.next().await {
                break id;
            }
        };
        assert!(run.asking(), "an unclaimed delegation waits for a client");
        assert!(run.claimed(id).is_none());
        assert!(!run.finish(id, Ok("early".into())), "only a claimed one finishes");

        assert!(run.claim(id));
        assert!(!run.claim(id), "claimed only once");
        assert!(!run.asking());
        assert_eq!(run.claimed(id).map(|claimed| claimed.label).as_deref(), Some("survey"));
        let joined: Vec<Event> = run.events().take(1).collect().await;
        assert_eq!(names(&joined), ["began"], "a late client is not offered it");

        assert!(run.finish(id, Ok("found it".into())));
        assert_eq!(delegated.await.unwrap(), Ok("found it".into()));
        assert!(run.claimed(id).is_none());
    }

    #[tokio::test]
    async fn a_withdrawn_delegation_stops_waiting() {
        let run = Run::new("hi".into(), 0, || {});
        let task = Task {
            agent: AgentId("a1".into()),
            label: "survey".into(),
            machine: "laptop".into(),
            directory: None,
            message: "look around".into(),
            follow_up: false,
            model: "openai/gpt-5.5".into(),
            thinking: None,
        };
        let mut delegated = Box::pin(run.delegate(task));
        assert!(futures::poll!(&mut delegated).is_pending());
        assert!(run.asking());
        drop(delegated);
        assert!(!run.asking(), "withdrawn once its parent's call is dropped");
    }
}
