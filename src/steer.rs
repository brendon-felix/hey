/* -------------------------------------------------------------------------- */
/*                                  steer.rs                                  */
/* -------------------------------------------------------------------------- */

//! Messages the user sends to an answer in progress. Rather than stop the
//! answer and start over, the host hands each to the model inside the next
//! tool result, as a block marked so every client shows it as the user's
//! message again; an answer that ends first takes it as its next prompt.

use std::sync::Arc;

use rig_agent::agent::{AgentHook, HookContext, OutcomeAction, OutcomeEvent};
use rig_core::message::{Message, ToolResult, ToolResultContent, UserContent};
use rig_core::tool::ToolOutput;
use serde::{Deserialize, Serialize};

use crate::host::Run;
use crate::mention::is_context;
use crate::protocol::Attachment;

const OPEN: &str = "<user_message>\nThe user sent this while you were working. \
Take it into account as you go on:\n\n";
const CLOSE: &str = "\n</user_message>";

/// A message for the run in progress, as a client sends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steer {
    pub prompt: String,
    /// What its mentions attach from the user's working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A message the model has yet to be handed, its mentions resolved.
#[derive(Debug)]
pub struct Steering {
    pub prompt: String,
    /// What its mentions attach, as [`crate::mention::Context`] words it.
    pub context: Option<String>,
}

impl Steering {
    fn texts(self, prompt: impl FnOnce(String) -> String) -> impl Iterator<Item = String> {
        std::iter::once(prompt(self.prompt)).chain(self.context)
    }
}

/// `waiting` as the prompt of an answer that ended before taking them.
pub fn message(waiting: Vec<Steering>) -> Message {
    let content = waiting
        .into_iter()
        .flat_map(|steering| steering.texts(|prompt| prompt))
        .map(UserContent::text)
        .collect();
    Message::User { content }
}

/// The messages waiting for a run, until it takes no more.
#[derive(Debug)]
pub enum Waiting {
    Open(Vec<Steering>),
    Closed,
}

impl Default for Waiting {
    fn default() -> Self {
        Self::Open(Vec::new())
    }
}

impl Waiting {
    /// False once the run takes no more.
    pub fn push(&mut self, steering: Steering) -> bool {
        match self {
            Self::Open(waiting) => waiting.push(steering),
            Self::Closed => return false,
        }
        true
    }

    pub fn take(&mut self) -> Vec<Steering> {
        match self {
            Self::Open(waiting) => std::mem::take(waiting),
            Self::Closed => Vec::new(),
        }
    }

    /// What waits as the answer ends; when nothing does, the run takes no
    /// more, so none is accepted that it would never hand over.
    pub fn take_last(&mut self) -> Vec<Steering> {
        let waiting = self.take();
        if waiting.is_empty() {
            *self = Self::Closed;
        }
        waiting
    }

    pub fn close(&mut self) {
        *self = Self::Closed;
    }
}

/// Hands the model what waits for `run` with the next tool result.
pub struct Delivery(pub Arc<Run>);

impl AgentHook for Delivery {
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        let Some(result) = event.tool_result() else {
            return OutcomeAction::proceed();
        };
        let waiting = self.0.take_steers();
        if waiting.is_empty() {
            return OutcomeAction::proceed();
        }
        let added = waiting.into_iter().flat_map(|steering| steering.texts(|prompt: String| block(&prompt)));
        let content = result.output().as_content().iter().cloned().chain(added.map(ToolResultContent::text));
        match ToolOutput::content(content.collect()) {
            Ok(output) => OutcomeAction::rewrite_tool_output(&event, output),
            Err(_) => OutcomeAction::proceed(),
        }
    }
}

/// `prompt` as the model reads it in a tool result.
fn block(prompt: &str) -> String {
    format!("{OPEN}{prompt}{CLOSE}")
}

/// `result` as its tool gave it, and the messages it carried to the model,
/// as the user typed them. What their mentions attached is left out of both.
pub fn split(mut result: ToolResult) -> (ToolResult, Vec<String>) {
    let mut prompts = Vec::new();
    result.content.retain(|content| {
        let Some(text) = content.as_text() else {
            return true;
        };
        match prompt(text) {
            Some(prompt) => prompts.push(prompt.to_owned()),
            None if is_context(text) && !prompts.is_empty() => {}
            None => return true,
        }
        false
    });
    (result, prompts)
}

/// The messages `result` carried to the model, as the user typed them.
pub fn prompts(result: &ToolResult) -> impl Iterator<Item = &str> {
    result.content.iter().filter_map(ToolResultContent::as_text).filter_map(prompt)
}

/// The message `text` carries, when it is one.
fn prompt(text: &str) -> Option<&str> {
    text.strip_prefix(OPEN)?.strip_suffix(CLOSE)
}


#[cfg(test)]
mod tests {
    use rig_core::message::{CallId, ToolName};

    use super::*;
    use crate::mention::Context;

    #[test]
    fn splits_the_users_messages_and_their_context_from_what_the_tool_gave() {
        let attached = Attachment::File {
            path: "notes.md".into(),
            text: "hi".into(),
        };
        let mut context = Context::default();
        context.attach(&attached);
        let rig_core::message::Message::User { content } = context.message("use @notes.md") else {
            unreachable!()
        };
        let context = content.into_iter().nth(1).and_then(|content| match content {
            rig_core::message::UserContent::Text(text) => Some(text.text),
            _ => None,
        });
        let result = ToolResult {
            call: CallId::from_wire("1"),
            name: ToolName::new("read").unwrap(),
            content: ["file contents".to_owned(), block("use @notes.md")]
                .into_iter()
                .chain(context)
                .chain([block("and be quick")])
                .map(ToolResultContent::text)
                .collect(),
            is_error: false,
        };
        let (given, prompts) = split(result);
        assert_eq!(prompts, ["use @notes.md", "and be quick"]);
        let given: Vec<_> = given.content.iter().filter_map(ToolResultContent::as_text).collect();
        assert_eq!(given, ["file contents"]);
    }
}
