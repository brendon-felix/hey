/* -------------------------------------------------------------------------- */
/*                                  turn.rs                                   */
/* -------------------------------------------------------------------------- */

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::cursor;
use futures::StreamExt;
use tokio::task::JoinSet;
use tokio::time::{Interval, MissedTickBehavior};

use crate::backend::{Backend, Events};
use crate::mention;
use crate::notify;
use crate::protocol::{
    AgentId, ApprovalId, Ask, Attachment, Ending, Event, FollowerId, RunId, RunOptions, RunRequest,
    Task,
};
use crate::render::{self, Stream, Style, screen};
use crate::sessions::SessionId;
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Answered,
    Interrupted,
    Failed,
}

pub struct Turn {
    pub outcome: Outcome,
    /// Tokens the last model call took in and gave back: how full the
    /// context is.
    pub context: Option<u64>,
    /// The run shown, once its first event arrived.
    pub run: Option<RunId>,
    /// How many messages the thread held once the run ended.
    pub saved: Option<usize>,
    /// Whether the thread was left in plan mode, when the run changed it.
    pub plan: Option<bool>,
    /// How long the run took, when the host said when it began.
    pub elapsed: Option<Duration>,
}

/// A request to answer `prompt`, with the files and directories it mentions
/// in the user's working directory; the host attaches the rest, and tells
/// every client following the run what was attached.
pub fn request(prompt: String, options: RunOptions) -> RunRequest {
    RunRequest {
        attachments: attachments(&prompt),
        prompt,
        options,
        client: None,
        task: Task::Answer,
        goal: None,
        surface: crate::protocol::Surface::current(),
    }
}

/// A request to compact the thread, `focus` saying what its summary should
/// keep above all.
pub fn compaction(focus: String, options: RunOptions) -> RunRequest {
    RunRequest {
        prompt: focus,
        attachments: Vec::new(),
        options,
        client: None,
        task: Task::Compact,
        goal: None,
        surface: crate::protocol::Surface::current(),
    }
}

/// The skills of this machine that the host does not have, as the prompt
/// being sent may invoke them: the host cannot read their files, so they
/// travel with the message.
static CLIENT_SKILLS: std::sync::Mutex<Vec<crate::skills::Skill>> = std::sync::Mutex::new(Vec::new());

/// Sets the skills of this machine that the host does not have.
pub fn set_client_skills(skills: Vec<crate::skills::Skill>) {
    *CLIENT_SKILLS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = skills;
}

/// The client-side skills `prompt` invokes with `/name` or `@skill:name`,
/// each once, their instructions read now.
fn skill_attachments(prompt: &str) -> Vec<Attachment> {
    let skills = CLIENT_SKILLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if skills.is_empty() {
        return Vec::new();
    }
    let invoked = crate::skills::invoked(prompt).map(str::to_owned);
    let slashed = crate::skills::slash_names(prompt)
        .into_iter()
        .map(|(_, name)| name.to_owned());
    let mentioned = mention::mentions(prompt).filter_map(|found| {
        found
            .resolve(|reference| match reference {
                mention::Reference::Skill(name) => Some(name.to_owned()),
                _ => None,
            })
            .map(|(name, _)| name)
    });
    let mut attached: Vec<Attachment> = Vec::new();
    for name in invoked.into_iter().chain(slashed).chain(mentioned) {
        let Some(skill) = skills.iter().find(|skill| skill.name.as_str() == name) else {
            continue;
        };
        if attached.iter().any(|a| a.name() == format!("skill:{name}")) {
            continue;
        }
        match skill.content() {
            Ok(content) => attached.push(Attachment::Skill { name, content }),
            Err(error) => render::notice(&format!("skill:{name} not attached: {error:#}")),
        }
    }
    attached
}

/// What `prompt` mentions in the user's working directory, as read now, and
/// the skills of this machine it invokes.
pub fn attachments(prompt: &str) -> Vec<Attachment> {
    let skills = skill_attachments(prompt);
    skills.into_iter().chain(local_attachments(prompt)).collect()
}

fn local_attachments(prompt: &str) -> Vec<Attachment> {
    mention::local(prompt)
        .into_iter()
        .filter_map(|item| {
            item.load()
                .inspect_err(|error| {
                    tracing::warn!(error = format!("{error:#}"), "{item} not attached");
                    render::notice(&format!("{item} not attached: {error:#}"))
                })
                .ok()
        })
        .collect()
}

/// Starts answering `request` in `thread` and follows the run to its end,
/// which Ctrl+C brings early.
pub async fn run(
    backend: &Backend,
    thread: &SessionId,
    request: RunRequest,
    style: &Style,
) -> Turn {
    match start(backend, thread, request, style).await {
        Ok(follow) => follow_to_end(backend, thread, follow).await,
        Err(error) => {
            render::error(&format!("{error:#}"));
            screen::line("");
            Turn {
                outcome: Outcome::Failed,
                context: None,
                run: None,
                saved: None,
                plan: None,
                elapsed: None,
            }
        }
    }
}

/// Starts answering `request` in `thread`, to follow the run.
pub async fn start(
    backend: &Backend,
    thread: &SessionId,
    request: RunRequest,
    style: &Style,
) -> anyhow::Result<Follow> {
    let start = backend.start(thread, request);
    tokio::pin!(start);
    // Abandoning a remote start could leave its run going unseen, so Ctrl+C
    // while starting lets the run start, then stops it at once.
    let (started, interrupted) = tokio::select! {
        started = &mut start => (started, false),
        Ok(()) = tokio::signal::ctrl_c() => (start.await, true),
    };
    let mut follow = Follow::new(started?, style);
    if interrupted {
        follow.stop(backend, thread).await;
    }
    Ok(follow)
}

/// Follows a run to its end, answering its approvals, until Ctrl+C stops
/// it; a second Ctrl+C stops waiting for it to end.
pub async fn follow_to_end(backend: &Backend, thread: &SessionId, mut follow: Follow) -> Turn {
    print!("{}", cursor::Hide);
    let outcome = loop {
        let step = tokio::select! {
            step = follow.next(backend, thread) => step,
            Ok(()) = tokio::signal::ctrl_c() => {
                // Moves past the `^C` the terminal echoed.
                screen::line("");
                match follow.stopping() {
                    true => break Outcome::Interrupted,
                    false => follow.stop(backend, thread).await,
                }
                continue;
            }
        };
        match step {
            Step::Drawn | Step::Steered(_) => {}
            Step::Ask { id, ask } => follow.approve(backend, thread, id, &ask).await,
            Step::Ended(outcome) => break outcome,
        }
    };
    print!("{}", cursor::Show);
    follow.end(backend, thread, outcome).await
}

/// Tells the host the thread was shown up to its latest answer.
pub async fn viewed(backend: &Backend, thread: &SessionId) {
    if let Err(error) = backend.viewed(thread).await {
        tracing::warn!(
            thread = thread.as_str(),
            error = format!("{error:#}"),
            "still marked as new"
        );
        render::notice(&format!("still marked as new: {error:#}"));
    }
}

/// What following a run learned besides its outcome.
#[derive(Default)]
struct Seen {
    context: Option<u64>,
    run: Option<RunId>,
    /// How many messages the thread held as the run ended, once its end
    /// arrived rather than its events stopping.
    saved: Option<usize>,
    /// The labels of the run's subagents, which their events are shown under.
    agents: HashMap<AgentId, String>,
    /// This terminal's view of the run, when it runs tool calls for the host.
    follower: Option<FollowerId>,
    /// Plan mode, as the run last changed it.
    plan: Option<bool>,
}

impl Seen {
    fn label<'a>(&'a self, agent: &'a AgentId) -> &'a str {
        self.agents
            .get(agent)
            .map_or(agent.as_str(), String::as_str)
    }
}

/// What the run's events gave next: an event, a failure, or their end.
type Received = Option<anyhow::Result<Event>>;

/// What following a run came to, a step at a time.
pub enum Step {
    /// Something was drawn, or nothing needed to be.
    Drawn,
    /// The run waits for this terminal, or another, to decide `ask`.
    Ask {
        id: ApprovalId,
        ask: Ask,
    },
    /// The model was handed a message the user sent meanwhile, now shown.
    Steered(String),
    Ended(Outcome),
}

/// A run as this terminal follows it: its events drawn as they arrive, and
/// the tool calls it runs for the host on this machine.
pub struct Follow {
    events: Events,
    stream: Stream,
    seen: Seen,
    frames: Interval,
    /// What arrived while an approval was asked, shown once it is decided.
    held: VecDeque<Received>,
    /// Tool calls this machine runs for the run, stopped if it is let go.
    lent_calls: JoinSet<()>,
    /// Asked to stop: the rest is waited for, not drawn.
    stopping: bool,
    style: Style,
    /// When the run began, as the host's clock had it, once it said.
    began: Option<Instant>,
    notify_after: Option<Duration>,
}

impl Follow {
    pub fn new(events: Events, style: &Style) -> Self {
        let mut frames = tokio::time::interval(render::FRAME);
        frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            events,
            stream: Stream::new(style),
            seen: Seen::default(),
            frames,
            held: VecDeque::new(),
            lent_calls: JoinSet::new(),
            stopping: false,
            style: style.clone(),
            began: None,
            notify_after: style.notify_after,
        }
    }

    pub fn stopping(&self) -> bool {
        self.stopping
    }

    /// Waits for the run's next event, or the next frame, and draws it.
    /// Dropped while it waits, nothing is lost.
    pub async fn next(&mut self, backend: &Backend, thread: &SessionId) -> Step {
        let received = match self.held.pop_front() {
            Some(received) => received,
            None => tokio::select! {
                received = self.events.next() => received,
                _ = self.frames.tick() => {
                    if !self.stopping {
                        self.stream.tick();
                    }
                    return Step::Drawn;
                }
            },
        };
        match self.stopping {
            true => self.wait(received),
            false => self.draw(backend, thread, received),
        }
    }

    /// Asks `ask` until this client or another decides, so every client
    /// stops asking at once; what arrives meanwhile is drawn after.
    pub async fn approve(
        &mut self,
        backend: &Backend,
        thread: &SessionId,
        id: ApprovalId,
        ask: &Ask,
    ) {
        self.stream.settle();
        let held = approve(backend, thread, &mut self.events, id, ask, &self.style).await;
        self.held.extend(held);
    }

    /// Asks the host to stop the run, whose end is then waited for without
    /// drawing the rest, so the thread is free for the next message.
    pub async fn stop(&mut self, backend: &Backend, thread: &SessionId) {
        if self.stopping {
            return;
        }
        self.stream.settle();
        self.stopping = true;
        if let Err(error) = backend.stop(thread).await {
            render::error(&format!("{error:#}"));
        }
    }

    /// What following the run learned, once it ended with `outcome`. A run
    /// seen to its end is no longer new to the host.
    pub async fn end(mut self, backend: &Backend, thread: &SessionId, outcome: Outcome) -> Turn {
        self.stream.settle();
        if let Outcome::Interrupted = outcome {
            render::notice("interrupted");
        }
        if self.seen.saved.is_some() {
            viewed(backend, thread).await;
        }
        screen::line("");
        Turn {
            outcome,
            context: self.seen.context,
            run: self.seen.run,
            saved: self.seen.saved,
            plan: self.seen.plan,
            elapsed: self.began.map(|began| began.elapsed()),
        }
    }

    /// Draws an event, tool calls included.
    fn draw(&mut self, backend: &Backend, thread: &SessionId, received: Received) -> Step {
        let event = match received {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                self.stream.settle();
                render::error(&format!("{error:#}"));
                return Step::Ended(Outcome::Failed);
            }
            None => {
                self.stream.settle();
                render::error(&format!(
                    "the run's events stopped before it ended; `/resume {}` follows it again",
                    thread.as_str()
                ));
                return Step::Ended(Outcome::Failed);
            }
        };
        let stream = &mut self.stream;
        match event {
            Event::Began {
                run,
                elapsed,
                follower,
            } => {
                self.seen.run = Some(run);
                self.seen.follower = follower;
                self.began = Instant::now().checked_sub(elapsed);
                stream.began(elapsed);
            }
            Event::Text { text } => stream.text(text),
            Event::Reasoning { text } => stream.reasoning(text),
            Event::PartEnd => stream.end_part(),
            Event::Output { text } => stream.output(text),
            Event::Thought { duration } => stream.thought(Some(duration)),
            Event::ToolCallStart { name } => stream.preparing(name),
            Event::ToolCallArguments { json } => stream.arguments(&json),
            Event::Decided { .. } | Event::Claimed { .. } => {}
            Event::Context { tokens } => self.seen.context = Some(tokens),
            Event::Plan { on } => {
                stream.settle();
                self.seen.plan = Some(on);
                render::notice(if on { "plan mode" } else { "left plan mode" });
            }
            Event::Compacted {
                summary,
                before,
                after,
            } => {
                // Until a model call reports the real size, the summary's is
                // the best guess; a standalone `/compact` makes no call.
                self.seen.context = Some(after);
                stream.compacted(&summary, Some((before, after)));
            }
            Event::Approval { id, ask } => {
                stream.settle();
                notify::notify(&waiting(&ask));
                return Step::Ask { id, ask };
            }
            Event::Steered { prompt } => {
                stream.settle();
                render::user_message(&prompt);
                return Step::Steered(prompt);
            }
            Event::ClientCall { id, call } => {
                let lent = self
                    .seen
                    .follower
                    .and_then(|follower| backend.lend(thread, follower, id, call));
                if let Some(call) = lent {
                    self.lent_calls.spawn(call);
                }
            }
            Event::AgentStarted { agent, label } => {
                stream.settle();
                render::agent_started(&label);
                self.seen.agents.insert(agent, label);
            }
            Event::Agent { agent, step } => {
                if let Some(step) = render::agent_step(self.seen.label(&agent), &step) {
                    stream.agent_step(step);
                }
            }
            Event::ToolCall {
                name,
                args,
                machine,
                call,
            } => stream.call(&name, &args, machine.as_deref(), call),
            Event::ToolResult { result } => stream.result(&result),
            Event::AgentEnded { agent, error } => {
                stream.settle();
                render::agent_ended(self.seen.label(&agent), error.as_deref());
            }
            event => {
                stream.settle();
                if let Event::End { saved, .. } = &event {
                    self.seen.saved = Some(*saved);
                }
                if let Some(outcome) = report(event) {
                    if outcome != Outcome::Interrupted && long(self.began, self.notify_after) {
                        notify::notify(match outcome {
                            Outcome::Failed => "the answer failed",
                            _ => "answered",
                        });
                    }
                    return Step::Ended(outcome);
                }
            }
        }
        Step::Drawn
    }

    /// Notes what a stopped run's event says of it, until its end.
    fn wait(&mut self, received: Received) -> Step {
        match received {
            Some(Ok(Event::Began { run, .. })) => self.seen.run = Some(run),
            Some(Ok(Event::End { saved, .. })) => {
                self.seen.saved = Some(saved);
                return Step::Ended(Outcome::Interrupted);
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Step::Ended(Outcome::Interrupted),
        }
        Step::Drawn
    }
}

/// Whether a run that `began` went on longer than `after`.
fn long(began: Option<Instant>, after: Option<Duration>) -> bool {
    began
        .zip(after)
        .is_some_and(|(began, after)| began.elapsed() > after)
}

/// What a notification says of `ask`.
fn waiting(ask: &Ask) -> String {
    match ask {
        Ask::Tool { name, .. } => format!("`{name}` waits for approval"),
        Ask::Directory { path, .. } => format!("waits to use {path}"),
        Ask::Question { .. } => "has a question for you".into(),
        Ask::Plan { .. } => "has a plan for you to approve".into(),
    }
}

/// Asks `ask` until this client or another decides, so every client stops
/// asking at once. Returns what arrived meanwhile.
async fn approve(
    backend: &Backend,
    thread: &SessionId,
    events: &mut Events,
    id: ApprovalId,
    ask: &Ask,
    style: &Style,
) -> Vec<Received> {
    let mut held = Vec::new();
    let question = ui::confirm(ask, style);
    tokio::pin!(question);
    let decision = loop {
        tokio::select! {
            decision = &mut question => break Some(decision),
            received = events.next() => {
                let settled = match &received {
                    Some(Ok(Event::Decided { id: decided })) => *decided == id,
                    Some(Ok(Event::End { .. }) | Err(_)) | None => true,
                    Some(Ok(_)) => false,
                };
                held.push(received);
                if settled {
                    break None;
                }
            }
        }
    };
    if let Some(decision) = decision {
        match backend.answer(thread, id, decision).await {
            Ok(true) => {}
            Ok(false) => render::notice("already decided elsewhere"),
            Err(error) => render::error(&format!("{error:#}")),
        }
    }
    held
}

/// Prints an event that is not part of the reply's text, and how the run
/// ended if it did: only its end returns an outcome.
fn report(event: Event) -> Option<Outcome> {
    match event {
        Event::Notice { text } => render::notice(&text),
        Event::Attached { name } => render::attached(&name),
        Event::End { ending, .. } => {
            return Some(match ending {
                Ending::Answered => Outcome::Answered,
                Ending::Interrupted => Outcome::Interrupted,
                Ending::Failed { error } => {
                    render::error(&error);
                    Outcome::Failed
                }
            });
        }
        Event::Began { .. }
        | Event::Text { .. }
        | Event::Reasoning { .. }
        | Event::PartEnd
        | Event::Output { .. }
        | Event::Thought { .. }
        | Event::ToolCallStart { .. }
        | Event::ToolCallArguments { .. }
        | Event::ToolCall { .. }
        | Event::ToolResult { .. }
        | Event::Approval { .. }
        | Event::Decided { .. }
        | Event::Context { .. }
        | Event::Compacted { .. }
        | Event::Plan { .. }
        | Event::AgentStarted { .. }
        | Event::Agent { .. }
        | Event::AgentEnded { .. }
        | Event::ClientCall { .. }
        | Event::Steered { .. }
        | Event::Claimed { .. } => {}
    }
    None
}
