/* -------------------------------------------------------------------------- */
/*                                  turn.rs                                   */
/* -------------------------------------------------------------------------- */

use crossterm::cursor;
use futures::StreamExt;

use crate::backend::{Backend, Events};
use crate::mention;
use crate::protocol::{Attachment, Ending, Event, RunOptions, RunRequest};
use crate::render::{self, Reply, Style};
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
    match backend.start(thread, request).await {
        Ok(events) => follow(backend, thread, events, style).await,
        Err(error) => {
            render::error(&format!("{error:#}"));
            println!();
            Turn {
                outcome: Outcome::Failed,
                context: None,
            }
        }
    }
}

/// Draws a run's events, tool calls included, and answers its approvals,
/// until it finishes, fails, or Ctrl+C stops it.
pub async fn follow(backend: &Backend, thread: &SessionId, mut events: Events, style: &Style) -> Turn {
    let mut context = None;
    print!("{}", cursor::Hide);
    // Racing the whole drawing, rather than each wait for an event, lets
    // Ctrl+C cut a reply that is still animating events received long ago.
    let shown = tokio::select! {
        outcome = show(backend, thread, &mut events, style, &mut context) => Some(outcome),
        Ok(()) = tokio::signal::ctrl_c() => None,
    };
    let outcome = match shown {
        Some(outcome) => outcome,
        None => {
            println!();
            stop(backend, thread, events).await
        }
    };
    if let Outcome::Interrupted = outcome {
        render::notice("interrupted");
    }
    print!("{}", cursor::Show);
    println!();
    Turn { outcome, context }
}

async fn show(
    backend: &Backend,
    thread: &SessionId,
    events: &mut Events,
    style: &Style,
    context: &mut Option<u64>,
) -> Outcome {
    let mut reply = Reply::new(style);
    let outcome = loop {
        let event = match events.next().await {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                reply.finish().await;
                render::error(&format!("{error:#}"));
                break Outcome::Failed;
            }
            None => {
                reply.finish().await;
                render::error(&"the run's events stopped before it ended; `/resume` follows it again");
                break Outcome::Failed;
            }
        };
        match event {
            Event::Text { text } => reply.text(&text).await,
            Event::Reasoning { text } => reply.reasoning(&text).await,
            Event::PartEnd => reply.finish().await,
            Event::ToolCall { name, args } => {
                reply.finish().await;
                render::tool_call(&name, &args);
            }
            Event::Output { text } => render::live_output(&text),
            Event::ToolResult { result } => render::tool_result(&result),
            Event::Notice { text } => {
                reply.finish().await;
                render::notice(&text);
            }
            Event::Attached { name } => {
                reply.finish().await;
                render::attached(&name);
            }
            Event::Approval { id, tool } => {
                reply.finish().await;
                let decision = ui::confirm_tool(tool).await;
                match backend.answer(thread, id, decision).await {
                    Ok(true) => {}
                    Ok(false) => render::notice("already decided elsewhere"),
                    Err(error) => render::error(&format!("{error:#}")),
                }
            }
            Event::Decided { .. } => {}
            Event::Context { tokens } => *context = Some(tokens),
            Event::End { ending } => {
                break match ending {
                    Ending::Answered => Outcome::Answered,
                    Ending::Interrupted => Outcome::Interrupted,
                    Ending::Failed { error } => {
                        reply.finish().await;
                        render::error(&error);
                        Outcome::Failed
                    }
                };
            }
        }
    };
    reply.finish().await;
    outcome
}

/// Stops the run, then waits for its end without drawing the rest, so the
/// thread is free for the next message.
async fn stop(backend: &Backend, thread: &SessionId, mut events: Events) -> Outcome {
    if let Err(error) = backend.stop(thread).await {
        render::error(&format!("{error:#}"));
        return Outcome::Interrupted;
    }
    while let Some(Ok(event)) = events.next().await {
        if let Event::End { .. } = event {
            break;
        }
    }
    Outcome::Interrupted
}

