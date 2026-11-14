/* -------------------------------------------------------------------------- */
/*                                transcript.rs                               */
/* -------------------------------------------------------------------------- */

//! What was said in a conversation, as the Markdown it was written in: what
//! the user typed and what the model answered, without reasoning, tool calls,
//! their results, or what mentions attached.

use rig_core::message::{AssistantContent, Message, UserContent};

use crate::mention::is_context;
use crate::steer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speaker {
    User,
    Assistant,
}

impl Speaker {
    fn heading(self) -> &'static str {
        match self {
            Speaker::User => "## User",
            Speaker::Assistant => "## Assistant",
        }
    }
}

/// The whole conversation, a heading over each message, as Zed copies a
/// thread.
pub fn thread(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(said)
        .map(|(speaker, texts)| format!("{}\n\n{}", speaker.heading(), texts.join("\n\n")))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The latest reply: all the model wrote after the prompt it answered, as
/// Zed copies an agent response.
pub fn last_reply(messages: &[Message]) -> Option<String> {
    let said: Vec<_> = messages.iter().filter_map(said).collect();
    let end = said.iter().rposition(|(speaker, _)| *speaker == Speaker::Assistant)? + 1;
    let start = said[..end]
        .iter()
        .rposition(|(speaker, _)| *speaker == Speaker::User)
        .map_or(0, |prompt| prompt + 1);
    let texts: Vec<&str> = said[start..end].iter().flat_map(|(_, texts)| texts).copied().collect();
    Some(texts.join("\n\n"))
}

/// Who spoke in `message` and what they said, unless it says nothing, as one
/// carrying only tool results does; those results may carry what the user
/// sent while the model worked.
fn said(message: &Message) -> Option<(Speaker, Vec<&str>)> {
    let (speaker, texts): (_, Vec<&str>) = match message {
        Message::System { .. } => return None,
        Message::User { content } => (
            Speaker::User,
            content
                .iter()
                .flat_map(|content| match content {
                    UserContent::Text(text) if !is_context(&text.text) => vec![text.text.as_str()],
                    UserContent::ToolResult(result) => steer::prompts(result).collect(),
                    _ => Vec::new(),
                })
                .collect(),
        ),
        Message::Assistant(reply) => (
            Speaker::Assistant,
            reply
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect(),
        ),
    };
    let texts: Vec<&str> = texts
        .into_iter()
        .map(|text| text.trim_matches(['\n', '\r']))
        .filter(|text| !text.trim().is_empty())
        .collect();
    (!texts.is_empty()).then_some((speaker, texts))
}

#[cfg(test)]
mod tests {
    use rig_core::message::{AssistantMessage, CallId, ToolName};

    use super::*;

    fn assistant(content: Vec<AssistantContent>) -> Message {
        Message::Assistant(AssistantMessage::new(content))
    }

    #[test]
    fn last_reply_spans_the_turn_and_skips_what_was_not_said() {
        let read = ToolName::new("read").unwrap();
        let messages = [
            Message::user("first"),
            Message::assistant("old answer"),
            Message::user("second"),
            assistant(vec![
                AssistantContent::reasoning("hmm"),
                AssistantContent::text("\nLet me look.\n\n"),
                AssistantContent::tool_call("1", read.clone(), serde_json::json!({})),
            ]),
            Message::tool_result(CallId::from_wire("1"), read, "file contents"),
            assistant(vec![AssistantContent::text("A long line that the terminal wraps.\n\n```rs\nfn main() {}\n```")]),
            Message::user("stopped before any answer"),
        ];
        assert_eq!(
            last_reply(&messages).as_deref(),
            Some("Let me look.\n\nA long line that the terminal wraps.\n\n```rs\nfn main() {}\n```")
        );
        assert_eq!(last_reply(&messages[..1]), None);
    }
}
