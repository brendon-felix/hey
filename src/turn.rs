/* -------------------------------------------------------------------------- */
/*                                  turn.rs                                   */
/* -------------------------------------------------------------------------- */

use crossterm::cursor;
use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_core::completion::Usage;
use rig_core::streaming::{Item, StreamEvent};

use crate::agent::Assistant;
use crate::render::{self, Reply, Style};
use crate::sessions::SessionId;

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

/// Streams the agent's answer to `prompt`, tool calls included, until it
/// finishes, fails, or Ctrl+C cancels it.
pub async fn run(assistant: &Assistant, prompt: &str, session: &SessionId, style: &Style) -> Turn {
    let mut stream = assistant.run(prompt, session);
    let mut reply = Reply::new(style);
    let mut context = None;
    print!("{}", cursor::Hide);
    let outcome = loop {
        let item = tokio::select! {
            item = stream.next() => item,
            _ = tokio::signal::ctrl_c() => break Outcome::Interrupted,
        };
        match item {
            None => break Outcome::Answered,
            Some(Err(error)) => {
                reply.finish();
                render::error(&error);
                break Outcome::Failed;
            }
            Some(Ok(item)) => match item {
                MultiTurnStreamItem::StreamAssistantItem(Item::Event(event)) => match event {
                    StreamEvent::Text { text, .. } => reply.text(&text),
                    StreamEvent::Reasoning { text, .. } => reply.reasoning(&text),
                    StreamEvent::End { .. } => reply.finish(),
                    _ => {}
                },
                MultiTurnStreamItem::ToolResult { tool_result } => render::tool_result(&tool_result),
                MultiTurnStreamItem::CompletionCall(call) => context = tokens(&call.usage).or(context),
                MultiTurnStreamItem::ModelTurnRetried { .. } => {
                    reply.finish();
                    render::notice("retrying…");
                }
                _ => {}
            },
        }
    };
    drop(stream);
    reply.finish();
    if let Outcome::Interrupted = outcome {
        render::notice("interrupted");
    }
    print!("{}", cursor::Show);
    println!();
    Turn { outcome, context }
}

fn tokens(usage: &Usage) -> Option<u64> {
    usage.total_tokens.or(match (usage.input_tokens, usage.output_tokens) {
        (None, None) => None,
        (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
    })
}
