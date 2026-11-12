/* -------------------------------------------------------------------------- */
/*                                  turn.rs                                   */
/* -------------------------------------------------------------------------- */

use crossterm::cursor;
use futures::StreamExt;
use tokio::time::MissedTickBehavior;

use crate::backend::{Backend, Events};
use crate::mention;
use crate::protocol::{Attachment, Ending, Event, RunOptions, RunRequest};
use crate::render::{self, Stream, Style};
use crate::sessions::SessionId;
use crate::ui;

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
}

/// A request to answer `prompt`, with the files and directories it mentions
/// in the user's working directory; the host attaches the rest.
pub fn request(prompt: String, options: RunOptions) -> RunRequest {
    let attachments = mention::local(&prompt)
        .into_iter()
        .filter_map(|item| {
            render::attached(&item);
            item.load()
                .inspect_err(|error| render::notice(&format!("{item} not attached: {error:#}")))
                .ok()
        })
        .collect::<Vec<Attachment>>();
    RunRequest {
        prompt,
        attachments,
        options,
    }
}

/// Starts answering `request` in `thread` and follows the run to its end.
pub async fn run(backend: &Backend, thread: &SessionId, request: RunRequest, style: &Style) -> Turn {
    let start = backend.start(thread, request);
    tokio::pin!(start);
    // Abandoning a remote start could leave its run going unseen, so Ctrl+C
    // while starting lets the run start, then stops it at once.
    let (started, interrupted) = tokio::select! {
        started = &mut start => (started, false),
        Ok(()) = tokio::signal::ctrl_c() => (start.await, true),
    };
    match started {
        Ok(events) if interrupted => {
            let mut seen = Seen::default();
            let outcome = stop(backend, thread, events, &mut seen).await;
            end(backend, thread, outcome, seen).await
        }
        Ok(events) => follow(backend, thread, events, style).await,
        Err(error) => {
            render::error(&format!("{error:#}"));
            end(backend, thread, Outcome::Failed, Seen::default()).await
        }
    }
}

/// Draws a run's events, tool calls included, and answers its approvals,
/// until it finishes, fails, or Ctrl+C stops it. A run seen to its end
/// is no longer new to the host.
pub async fn follow(backend: &Backend, thread: &SessionId, mut events: Events, style: &Style) -> Turn {
    let mut seen = Seen::default();
    let mut stream = Stream::new(style);
    print!("{}", cursor::Hide);
    let shown = tokio::select! {
        outcome = show(backend, thread, &mut events, &mut stream, &mut seen) => Some(outcome),
        Ok(()) = tokio::signal::ctrl_c() => None,
    };
    stream.settle();
    let outcome = match shown {
        Some(outcome) => outcome,
        None => stop(backend, thread, events, &mut seen).await,
    };
    print!("{}", cursor::Show);
    end(backend, thread, outcome, seen).await
}

async fn end(backend: &Backend, thread: &SessionId, outcome: Outcome, seen: Seen) -> Turn {
    if let Outcome::Interrupted = outcome {
        render::notice("interrupted");
    }
    if seen.ended {
        viewed(backend, thread).await;
    }
    println!();
    Turn {
        outcome,
        context: seen.context,
    }
}

/// Tells the host the thread was shown up to its latest answer.
pub async fn viewed(backend: &Backend, thread: &SessionId) {
    if let Err(error) = backend.viewed(thread).await {
        render::notice(&format!("still marked as new: {error:#}"));
    }
}

/// What following a run learned besides its outcome.
#[derive(Default)]
struct Seen {
    context: Option<u64>,
    /// Whether the run's end arrived, rather than its events stopping.
    ended: bool,
}

async fn show(
    backend: &Backend,
    thread: &SessionId,
    events: &mut Events,
    stream: &mut Stream,
    seen: &mut Seen,
) -> Outcome {
    let mut frames = tokio::time::interval(render::FRAME);
    frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            event = events.next() => event,
            _ = frames.tick() => {
                stream.tick();
                continue;
            }
        };
        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                stream.settle();
                render::error(&format!("{error:#}"));
                return Outcome::Failed;
            }
            None => {
                stream.settle();
                render::error(&"the run's events stopped before it ended; `/resume` follows it again");
                return Outcome::Failed;
            }
        };
        match event {
            Event::Text { text } => stream.text(text),
            Event::Reasoning { text } => stream.reasoning(text),
            Event::PartEnd => stream.end_part(),
            Event::Decided { .. } => {}
            Event::Context { tokens } => seen.context = Some(tokens),
            event => {
                stream.settle();
                if let Some(outcome) = report(backend, thread, event).await {
                    seen.ended = true;
                    return outcome;
                }
            }
        }
    }
}

/// Prints an event that is not part of the reply's text, and how the run
/// ended if it did: only its end returns an outcome.
async fn report(backend: &Backend, thread: &SessionId, event: Event) -> Option<Outcome> {
    match event {
        Event::ToolCall { name, args } => render::tool_call(&name, &args),
        Event::Output { text } => render::live_output(&text),
        Event::ToolResult { result } => render::tool_result(&result),
        Event::Notice { text } => render::notice(&text),
        Event::Attached { name } => render::attached(&name),
        Event::Approval { id, tool } => {
            let decision = ui::confirm_tool(tool).await;
            match backend.answer(thread, id, decision).await {
                Ok(true) => {}
                Ok(false) => render::notice("already decided elsewhere"),
                Err(error) => render::error(&format!("{error:#}")),
            }
        }
        Event::End { ending } => {
            return Some(match ending {
                Ending::Answered => Outcome::Answered,
                Ending::Interrupted => Outcome::Interrupted,
                Ending::Failed { error } => {
                    render::error(&error);
                    Outcome::Failed
                }
            });
        }
        Event::Text { .. }
        | Event::Reasoning { .. }
        | Event::PartEnd
        | Event::Decided { .. }
        | Event::Context { .. } => {}
    }
    None
}

/// Stops the run, then waits for its end without drawing the rest, so the
/// thread is free for the next message.
async fn stop(backend: &Backend, thread: &SessionId, mut events: Events, seen: &mut Seen) -> Outcome {
    // Moves past the `^C` the terminal echoed.
    println!();
    if let Err(error) = backend.stop(thread).await {
        render::error(&format!("{error:#}"));
        return Outcome::Interrupted;
    }
    while let Some(Ok(event)) = events.next().await {
        if let Event::End { .. } = event {
            seen.ended = true;
            break;
        }
    }
    Outcome::Interrupted
}

