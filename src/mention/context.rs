/* -------------------------------------------------------------------------- */
/*                             mention/context.rs                             */
/* -------------------------------------------------------------------------- */

use std::path::Path;

use rig_core::message::{AssistantContent, Message, UserContent};

use crate::protocol::Attachment;
use crate::sessions::SessionId;

/// Opens the part of a user message that holds what its mentions attach, in
/// the form Zed's agent sends.
const OPEN_CONTEXT: &str = "<context>\nThe following items were attached by the user. \
They are up-to-date and don't need to be re-read.\n";

/// What a message's mentions attach, by kind, in the order attached.
#[derive(Default)]
pub struct Context {
    skills: Vec<String>,
    files: Vec<String>,
    directories: Vec<String>,
    threads: Vec<String>,
}

impl Context {
    pub fn attach(&mut self, attachment: &Attachment) {
        match attachment {
            Attachment::File { path, text } => self.files.push(code_block(path, text)),
            Attachment::Directory {
                path,
                entries,
                truncated,
            } => self.directories.push(listing(path, entries, *truncated)),
        }
    }

    /// A skill the user invoked, as [`Skill::content`](crate::skills::Skill::content)
    /// wraps it.
    pub fn skill(&mut self, content: String) {
        self.skills.push(content);
    }

    pub fn summary(&mut self, id: &SessionId, summary: &str) {
        self.threads
            .push(format!("Summary of conversation {id}\n\n{summary}"));
    }

    /// `prompt` as the model gets it: what was typed, then what was attached.
    pub fn message(self, prompt: &str) -> Message {
        let content = std::iter::once(UserContent::text(prompt))
            .chain(self.into_text().map(UserContent::text))
            .collect();
        Message::User { content }
    }

    fn into_text(self) -> Option<String> {
        let sections: Vec<String> = [
            ("skills", self.skills),
            ("files", self.files),
            ("directories", self.directories),
            ("threads", self.threads),
        ]
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(tag, items)| format!("<{tag}>\n{}\n</{tag}>", items.join("\n\n")))
        .collect();
        (!sections.is_empty())
            .then(|| format!("{OPEN_CONTEXT}\n{}\n</context>", sections.join("\n")))
    }
}

/// Whether `text` is the part of a user message that holds what its
/// mentions attached, rather than what the user typed.
pub fn is_context(text: &str) -> bool {
    text.starts_with(OPEN_CONTEXT)
}

/// What was said, without tool calls, their results, or attachments.
pub fn transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| {
            let (speaker, texts): (_, Vec<&str>) = match message {
                Message::System { .. } => return None,
                Message::User { content } => (
                    "User",
                    content
                        .iter()
                        .filter_map(|content| match content {
                            UserContent::Text(text) if !is_context(&text.text) => {
                                Some(text.text.as_str())
                            }
                            _ => None,
                        })
                        .collect(),
                ),
                Message::Assistant(reply) => (
                    "Assistant",
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
            (!texts.is_empty()).then(|| format!("## {speaker}\n\n{}", texts.join("\n\n")))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A fence longer than any run of backticks in `text`, tagged with the
/// extension and path, as Zed tags attached files.
fn code_block(path: &str, text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let tag = match Path::new(path).extension().and_then(|extension| extension.to_str()) {
        Some(extension) => format!("{extension} {path}"),
        None => path.to_owned(),
    };
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    format!("{fence}{tag}\n{text}{newline}{fence}")
}

fn listing(path: &str, entries: &[String], truncated: bool) -> String {
    let slash = if path.ends_with('/') { "" } else { "/" };
    let more = truncated.then_some("(more entries are not listed)");
    std::iter::once(format!("Contents of {path}{slash}"))
        .chain(entries.iter().cloned())
        .chain(more.map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n")
}
