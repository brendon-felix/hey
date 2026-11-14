/* -------------------------------------------------------------------------- */
/*                                host/run.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use futures::{Stream, StreamExt, stream};
use rig_agent::agent::MultiTurnStreamItem;
use rig_core::completion::Usage;
use rig_core::message::AssistantContent;
use rig_core::streaming::{Item, PartKind, StreamEvent};
use tokio::sync::{Notify, oneshot, watch};

use super::record::Record;
use crate::compaction;
use crate::protocol::{
    AgentId, ApprovalId, Ask, Call, CallId, CallResult, Decision, Event, FollowerId, RunId,
    Running,
};
use crate::steer::{self, Steering, Waiting};

/// A run in progress on a thread. The host owns it, not a client: clients
/// come and go, replaying and following its events, answering its
/// approvals and running its tool calls on their machines, while it
/// carries on.
pub struct Run {
    id: RunId,
    started: Instant,
    prompt: String,
    /// The model answering, as `vendor/model`.
    model: String,
    /// How many messages the thread held when the run began.
    history: usize,
    /// Messages the user sent meanwhile, for the model to be handed.
    steers: Mutex<Waiting>,
    /// Compactions asked for the next model call to make.
    compactions: Mutex<compaction::Requests>,
    log: watch::Sender<Vec<Event>>,
    approvals: Mutex<HashMap<ApprovalId, oneshot::Sender<Decision>>>,
    calls: Mutex<Calls>,
    next_id: AtomicU64,
    stop: Notify,
    /// What the run said, for when it ends without saving it.
    record: Record,
    /// Called when whether the run waits for a client changes.
    asking_changed: Box<dyn Fn() + Send + Sync>,
}

/// The tool calls clients run, and the clients that can: under one lock,
/// so a call is never handed to a client that has just left.
#[derive(Default)]
struct Calls {
    pending: HashMap<CallId, Pending>,
    /// The machine of each client following the run that runs tool calls.
    followers: HashMap<FollowerId, String>,
}

struct Pending {
    machine: String,
    agent: Option<AgentId>,
    /// The client running it; until one takes it up, any on its machine may.
    claimed: Option<FollowerId>,
    done: oneshot::Sender<CallResult>,
}

/// A claimed call: what the client reports of it is shown under the
/// subagent making it, if one does.
pub struct Claimed {
    pub agent: Option<AgentId>,
}

impl Calls {
    fn followed_on(&self, machine: &str) -> bool {
        self.followers.values().any(|follower| follower == machine)
    }
}

impl Run {
    pub fn new(
        prompt: String,
        model: String,
        history: usize,
        asking_changed: impl Fn() + Send + Sync + 'static,
    ) -> Self {
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
            model,
            history,
            steers: Mutex::default(),
            compactions: Mutex::default(),
            log: watch::Sender::default(),
            approvals: Mutex::default(),
            calls: Mutex::default(),
            next_id: AtomicU64::new(0),
            stop: Notify::new(),
            record: Record::default(),
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

    pub fn record(&self) -> &Record {
        &self.record
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Has the model handed `steering` as soon as it can; false once the run
    /// takes no more.
    pub fn steer(&self, steering: Steering) -> bool {
        lock(&self.steers).push(steering)
    }

    /// The messages waiting, for the next tool result to hand over.
    pub fn take_steers(&self) -> Vec<Steering> {
        lock(&self.steers).take()
    }

    /// The messages waiting as an answer ends; none, and the run takes no more.
    pub fn last_steers(&self) -> Vec<Steering> {
        lock(&self.steers).take_last()
    }

    /// Takes no more messages, dropping those waiting.
    pub fn close_steers(&self) {
        lock(&self.steers).close();
    }

    /// Has the next model call compact first, `focus` saying what the
    /// summary should keep above all; false once the run makes no more.
    pub fn compact(&self, focus: &str) -> bool {
        lock(&self.compactions).push(focus)
    }

    /// The focus of the compaction asked for, if one was.
    pub fn take_compaction(&self) -> Option<String> {
        lock(&self.compactions).take()
    }

    /// Takes no more compactions, returning the one asked and not made.
    pub fn close_compactions(&self) -> Option<String> {
        lock(&self.compactions).close()
    }

    /// Whether the run waits for a client to decide something.
    pub fn asking(&self) -> bool {
        !lock(&self.approvals).is_empty()
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

    /// Has a client following the run on `call.machine` run `call` there,
    /// for `agent` when a subagent makes it. Fails at once when no client
    /// there follows the run, and when the one running it stops following.
    /// Dropped, as when the run stops, it is withdrawn.
    pub async fn call(&self, call: Call, agent: Option<AgentId>) -> CallResult {
        let id = CallId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (done, result) = oneshot::channel();
        {
            let mut calls = lock(&self.calls);
            if !calls.followed_on(&call.machine) {
                return Err(format!(
                    "no terminal on `{0}` is following this conversation, so the call could not \
                     run there; tools run on `{0}` only while the user has this conversation \
                     open in hey there",
                    call.machine
                ));
            }
            let pending = Pending {
                machine: call.machine.clone(),
                agent,
                claimed: None,
                done,
            };
            calls.pending.insert(id, pending);
        }
        let _withdraw = Withdraw { run: self, id };
        let machine = call.machine.clone();
        self.emit(Event::ClientCall { id, call });
        result.await.unwrap_or_else(|_| Err(went_away(&machine)))
    }

    /// Takes up call `id` for `follower`; false when another client took it
    /// first, it is no longer waited for, or it is for another machine.
    pub fn claim(&self, id: CallId, follower: FollowerId) -> bool {
        let claimed = {
            let mut calls = lock(&self.calls);
            let machine = calls.followers.get(&follower).cloned();
            match calls.pending.get_mut(&id) {
                Some(pending) if pending.claimed.is_none() && machine.as_ref() == Some(&pending.machine) => {
                    pending.claimed = Some(follower);
                    true
                }
                _ => false,
            }
        };
        if claimed {
            self.emit(Event::Claimed { id });
        }
        claimed
    }

    /// The subagent making a claimed call, while it is still waited for.
    pub fn claimed(&self, id: CallId) -> Option<Claimed> {
        lock(&self.calls)
            .pending
            .get(&id)
            .filter(|pending| pending.claimed.is_some())
            .map(|pending| Claimed {
                agent: pending.agent.clone(),
            })
    }

    /// Ends a claimed call with `result`; false when it was not waited for.
    pub fn finish(&self, id: CallId, result: CallResult) -> bool {
        let mut calls = lock(&self.calls);
        if !calls.pending.get(&id).is_some_and(|pending| pending.claimed.is_some()) {
            return false;
        }
        calls
            .pending
            .remove(&id)
            .is_some_and(|pending| pending.done.send(result).is_ok())
    }

    pub fn stop(&self) {
        self.stop.notify_one();
    }

    pub async fn stopped(&self) {
        self.stop.notified().await;
    }

    /// The events so far, then each as it happens, until the run ends. A
    /// client on `machine` that runs tool calls follows the run for as long
    /// as it holds the stream.
    pub fn events(self: &Arc<Self>, machine: Option<String>) -> impl Stream<Item = Event> + Send + 'static {
        let following = machine.map(|machine| {
            let id = FollowerId(self.next_id.fetch_add(1, Ordering::Relaxed));
            lock(&self.calls).followers.insert(id, machine);
            Following {
                run: Arc::downgrade(self),
                id,
            }
        });
        let (run, started) = (self.id, self.started);
        let follower = following.as_ref().map(|following| following.id);
        let began = stream::once(async move {
            Event::Began {
                run,
                elapsed: started.elapsed(),
                follower,
            }
        });
        let log = self.log.subscribe();
        // The stream holds `following`, so the client stops following the
        // run when it lets the stream go or the run ends.
        let logged = stream::unfold(Some((log, 0, following)), |state| async move {
            let (mut log, mut read, following) = state?;
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
                    return Some((batch, (!ended).then_some((log, read, following))));
                }
                log.changed().await.ok()?;
            }
        })
        .flat_map(stream::iter);
        began.chain(logged)
    }

    /// `follower` stopped following: the calls it took up fail, and so do
    /// those waiting for a client on its machine when it was the last.
    fn unfollow(&self, follower: FollowerId) {
        let mut calls = lock(&self.calls);
        let Some(machine) = calls.followers.remove(&follower) else {
            return;
        };
        let followed = calls.followed_on(&machine);
        let lost: Vec<CallId> = calls
            .pending
            .iter()
            .filter(|(_, pending)| match pending.claimed {
                Some(claimed) => claimed == follower,
                None => pending.machine == machine && !followed,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(pending) = calls.pending.remove(&id) {
                _ = pending.done.send(Err(went_away(&machine)));
            }
        }
    }
}

fn went_away(machine: &str) -> String {
    format!("the terminal on `{machine}` running the call stopped following the conversation")
}

/// A client's following of the run, ended when dropped.
struct Following {
    run: Weak<Run>,
    id: FollowerId,
}

impl Drop for Following {
    fn drop(&mut self) {
        if let Some(run) = self.run.upgrade() {
            run.unfollow(self.id);
        }
    }
}

/// Removes a call once nothing waits for it.
struct Withdraw<'a> {
    run: &'a Run,
    id: CallId,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        lock(&self.run.calls).pending.remove(&self.id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `events` without the approvals they also show decided and the calls
/// they show claimed, so a client that joins late is not asked about them.
fn undecided(events: &[Event]) -> Vec<Event> {
    let decided = |id: &ApprovalId| {
        events
            .iter()
            .any(|event| matches!(event, Event::Decided { id: other } if other == id))
    };
    let claimed = |id: &CallId| {
        events
            .iter()
            .any(|event| matches!(event, Event::Claimed { id: other } if other == id))
    };
    events
        .iter()
        .filter(|event| match event {
            Event::Approval { id, .. } => !decided(id),
            Event::ClientCall { id, .. } => !claimed(id),
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
    pub fn events(&mut self, item: MultiTurnStreamItem) -> Vec<Event> {
        match item {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(event)) => {
                self.thought_before(&event).into_iter().chain(self.part(event)).collect()
            }
            MultiTurnStreamItem::ToolResult { tool_result } => {
                let (result, prompts) = steer::split(tool_result);
                let steered = prompts.into_iter().map(|prompt| Event::Steered { prompt });
                std::iter::once(Event::ToolResult { result }).chain(steered).collect()
            }
            MultiTurnStreamItem::CompletionCall(call) => {
                tokens(&call.usage).map(|tokens| Event::Context { tokens }).into_iter().collect()
            }
            MultiTurnStreamItem::ModelTurnRetried { .. } => vec![Event::Notice {
                text: "retrying…".into(),
            }],
            _ => Vec::new(),
        }
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
                Event::ClientCall { call, .. } => format!("call {}", call.tool),
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
        let run = std::sync::Arc::new(Run::new("hi".into(), "test/model".into(), 0, || {}));
        let mut early = Box::pin(run.events(None));
        run.emit(Event::Text { text: "a".into() });
        let asking = tokio::spawn({
            let run = run.clone();
            async move {
                let ask = Ask::Tool {
                    name: "shell".into(),
                    agent: None,
                    machine: None,
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
        let joined: Vec<Event> = run.events(None).take(3).collect().await;
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
        let late: Vec<Event> = run.events(None).collect().await;
        assert_eq!(names(&late), ["began", "a", "decided", "end"]);
    }

    #[tokio::test]
    async fn every_client_times_the_run_from_when_it_began() {
        let run = std::sync::Arc::new(Run::new("hi".into(), "test/model".into(), 0, || {}));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let joined = Box::pin(run.events(None)).next().await;
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

    fn call(tool: &str) -> Call {
        Call {
            machine: "laptop".into(),
            tool: tool.into(),
            args: "{}".into(),
            agent: None,
        }
    }

    /// Follows `run` from `machine`, returning the stream and its follower.
    async fn follow(run: &Arc<Run>, machine: &str) -> (impl Stream<Item = Event> + Unpin, FollowerId) {
        let mut events = Box::pin(run.events(Some(machine.into())));
        match events.next().await {
            Some(Event::Began { follower: Some(follower), .. }) => (events, follower),
            other => panic!("{other:?}"),
        }
    }

    async fn next_call(events: &mut (impl Stream<Item = Event> + Unpin)) -> CallId {
        loop {
            if let Some(Event::ClientCall { id, .. }) = events.next().await {
                return id;
            }
        }
    }

    #[tokio::test]
    async fn a_call_fails_at_once_without_a_terminal_on_its_machine() {
        let run = Arc::new(Run::new("hi".into(), "test/model".into(), 0, || {}));
        let error = run.call(call("read"), None).await.unwrap_err();
        assert!(error.contains("no terminal on `laptop`"), "{error}");

        let (_desk, _) = follow(&run, "desk").await;
        let error = run.call(call("read"), None).await.unwrap_err();
        assert!(error.contains("no terminal on `laptop`"), "another machine's terminal cannot run it");
    }

    #[tokio::test]
    async fn one_terminal_on_the_machine_runs_a_call_and_its_leaving_fails_it() {
        let run = Arc::new(Run::new("hi".into(), "test/model".into(), 0, || {}));
        let (mut laptop, follower) = follow(&run, "laptop").await;
        let (_desk, desk) = follow(&run, "desk").await;

        let called = tokio::spawn({
            let run = run.clone();
            async move { run.call(call("read"), None).await }
        });
        let id = next_call(&mut laptop).await;
        assert!(run.claimed(id).is_none());
        assert!(!run.finish(id, Ok(Vec::new())), "only a claimed call finishes");
        assert!(!run.claim(id, desk), "a terminal on another machine cannot take it");
        assert!(run.claim(id, follower));
        assert!(!run.claim(id, follower), "taken only once");
        let joined: Vec<Event> = run.events(None).take(2).collect().await;
        assert_eq!(names(&joined), ["began", "claimed"], "a late terminal is not offered it");
        assert!(run.finish(id, Ok(Vec::new())));
        assert_eq!(called.await.unwrap(), Ok(Vec::new()));

        let called = tokio::spawn({
            let run = run.clone();
            async move { run.call(call("shell"), None).await }
        });
        let id = next_call(&mut laptop).await;
        assert!(run.claim(id, follower));
        drop(laptop);
        let error = called.await.unwrap().unwrap_err();
        assert!(error.contains("stopped following"), "{error}");
    }
}
