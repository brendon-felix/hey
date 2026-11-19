/* -------------------------------------------------------------------------- */
/*                                headless.rs                                 */
/* -------------------------------------------------------------------------- */

//! `hey --print`: one answer without the terminal, for scripts and cron.
//! Its final reply goes to stdout as it is, or, with `--json`, every event
//! of the run as a line of JSON. Nothing asks: a tool call that needs
//! confirming is declined, unless `--yes` allows it.

use std::io::Write;

use anyhow::{Result, bail};
use futures::StreamExt;
use tokio::task::JoinSet;

use crate::backend::Backend;
use crate::protocol::{Decision, Ending, Event, RunRequest};
use crate::sessions::SessionId;

/// How the run's events are written.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub json: bool,
    /// Allow every tool call that would ask.
    pub yes: bool,
}

/// What a run told, as far as printing goes.
#[derive(Default)]
struct Reply {
    /// The reply's text since the last tool call: the final reply.
    text: String,
}

impl Reply {
    fn take(&mut self, event: &Event) {
        match event {
            Event::Text { text } => self.text.push_str(text),
            Event::ToolCall { .. } | Event::ToolCallStart { .. } => self.text.clear(),
            _ => {}
        }
    }
}

/// Answers `request` in `thread`, printing as `options` say. Fails when the
/// answer did.
pub async fn run(backend: &Backend, thread: &SessionId, request: RunRequest, options: Options) -> Result<()> {
    let mut events = backend.start(thread, request).await?;
    let mut reply = Reply::default();
    let mut follower = None;
    let mut lent = JoinSet::new();
    let mut out = std::io::stdout().lock();
    let ending = loop {
        let event = tokio::select! {
            event = events.next() => event,
            _ = tokio::signal::ctrl_c() => {
                backend.stop(thread).await.ok();
                continue;
            }
        };
        let Some(event) = event else {
            bail!("the run's events stopped before it ended");
        };
        let event = event?;
        if options.json {
            writeln!(out, "{}", serde_json::to_string(&event)?)?;
            out.flush()?;
        }
        reply.take(&event);
        match event {
            Event::Began { follower: began, .. } => follower = began,
            Event::Approval { id, ask } => {
                let decision = decide(options.yes);
                if !options.json && decision == Decision::Decline {
                    eprintln!("hey: declined {ask:?}; --yes allows it");
                }
                backend.answer(thread, id, decision).await?;
            }
            Event::ClientCall { id, call } => {
                if let Some(call) = follower.and_then(|follower| backend.lend(thread, follower, id, call)) {
                    lent.spawn(call);
                }
            }
            Event::Notice { text } if !options.json => eprintln!("hey: {text}"),
            Event::End { ending, .. } => break ending,
            _ => {}
        }
    };
    lent.shutdown().await;
    match ending {
        Ending::Answered => {
            if !options.json {
                let text = reply.text.trim_end();
                writeln!(out, "{text}")?;
            }
            Ok(())
        }
        Ending::Interrupted => bail!("interrupted"),
        Ending::Failed { error } => bail!(error),
    }
}

/// What `prompt` mentions in the working directory, as read now; what
/// could not be attached is told on stderr.
pub fn attachments(prompt: &str) -> Vec<crate::protocol::Attachment> {
    crate::mention::local(prompt)
        .into_iter()
        .filter_map(|item| {
            item.load()
                .inspect_err(|error| eprintln!("hey: {item} not attached: {error:#}"))
                .ok()
        })
        .collect()
}

/// How a question no one can answer is decided.
fn decide(yes: bool) -> Decision {
    match yes {
        true => Decision::Once,
        false => Decision::Decline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_final_reply_is_the_text_after_the_last_tool_call() {
        let mut reply = Reply::default();
        for event in [
            Event::Text { text: "Let me look.".into() },
            Event::ToolCall {
                name: "read".into(),
                args: "{}".into(),
                machine: None,
                call: None,
            },
            Event::Text { text: "It is ".into() },
            Event::Text { text: "42.".into() },
        ] {
            reply.take(&event);
        }
        assert_eq!(reply.text, "It is 42.");
    }

    #[test]
    fn questions_are_declined_unless_yes() {
        assert_eq!(decide(false), Decision::Decline);
        assert_eq!(decide(true), Decision::Once);
    }
}
