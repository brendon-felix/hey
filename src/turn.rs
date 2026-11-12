/* -------------------------------------------------------------------------- */
/*                                  turn.rs                                   */
/* -------------------------------------------------------------------------- */

use crossterm::cursor;
use futures::StreamExt;

use crate::backend::{Backend, Events};
use crate::protocol::{Ending, Event, RunRequest};
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
    let mut reply = Reply::new(style);
    let mut context = None;
    print!("{}", cursor::Hide);
    let outcome = loop {
        let event = tokio::select! {
            event = events.next() => event,
            _ = tokio::signal::ctrl_c() => match backend.stop(thread).await {
                // The host ends the run and says so.
                Ok(_) => continue,
                Err(error) => {
                    reply.finish();
                    render::error(&format!("{error:#}"));
                    break Outcome::Interrupted;
                }
            },
        };
        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                reply.finish();
                render::error(&format!("{error:#}"));
                break Outcome::Failed;
            }
            None => {
                reply.finish();
                render::error(&"the run's events stopped before it ended; `/resume` follows it again");
                break Outcome::Failed;
            }
        };
        match event {
            Event::Text { text } => reply.text(&text),
            Event::Reasoning { text } => reply.reasoning(&text),
            Event::PartEnd => reply.finish(),
            Event::ToolCall { name, args } => {
                reply.finish();
                render::tool_call(&name, &args);
            }
            Event::Output { text } => render::live_output(&text),
            Event::ToolResult { result } => render::tool_result(&result),
            Event::Notice { text } => {
                reply.finish();
                render::notice(&text);
            }
            Event::Approval { id, tool } => {
                reply.finish();
                let decision = ui::confirm_tool(tool).await;
                match backend.answer(thread, id, decision).await {
                    Ok(true) => {}
                    Ok(false) => render::notice("already decided elsewhere"),
                    Err(error) => render::error(&format!("{error:#}")),
                }
            }
            Event::Decided { .. } => {}
            Event::Context { tokens } => context = Some(tokens),
            Event::End { ending } => {
                break match ending {
                    Ending::Answered => Outcome::Answered,
                    Ending::Interrupted => Outcome::Interrupted,
                    Ending::Failed { error } => {
                        reply.finish();
                        render::error(&error);
                        Outcome::Failed
                    }
                };
            }
        }
    };
    drop(events);
    reply.finish();
    if let Outcome::Interrupted = outcome {
        render::notice("interrupted");
    }
    print!("{}", cursor::Show);
    println!();
    Turn { outcome, context }
}

