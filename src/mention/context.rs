/* -------------------------------------------------------------------------- */
/*                             mention/context.rs                             */
/* -------------------------------------------------------------------------- */

use std::path::Path;

use rig_core::message::{Message, UserContent};

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
    /// Images, which follow the text as images.
    images: Vec<UserContent>,
}

impl Context {
    pub fn attach(&mut self, attachment: &Attachment) {
        match attachment {
            Attachment::File { path, text } => self.files.push(code_block(path, text)),
            Attachment::Image { media_type, data, .. } => {
                self.images
                    .push(UserContent::image_base64(data.clone(), media_type.clone(), None));
            }
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
    pub fn message(mut self, prompt: &str) -> Message {
        let images = std::mem::take(&mut self.images);
        let content = std::iter::once(UserContent::text(prompt))
            .chain(self.text().map(UserContent::text))
            .chain(images)
            .collect();
        Message::User { content }
    }

    /// Whether images were attached, which [`Self::text`] leaves out.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// What was attached, as the part of a message that follows the prompt;
    /// images are left out.
    pub fn text(self) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_follow_the_prompt_and_the_text_as_images() {
        let mut context = Context::default();
        context.attach(&Attachment::Image {
            path: "/tmp/a.png".into(),
            media_type: Some(rig_core::message::ImageMediaType::PNG),
            data: "iVBORw0KGgo=".into(),
        });
        assert!(context.has_images());
        let Message::User { content } = context.message("look") else {
            unreachable!()
        };
        let content: Vec<_> = content.into_iter().collect();
        assert_eq!(content.len(), 2, "the prompt, then the image");
        assert!(matches!(content[1], UserContent::Image(_)));
    }
}
