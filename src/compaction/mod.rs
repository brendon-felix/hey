/* -------------------------------------------------------------------------- */
/*                             compaction/mod.rs                              */
/* -------------------------------------------------------------------------- */

//! Compacting a conversation: its older messages are replaced by a summary
//! the model writes of them, keeping the newest as they are, as rig-memory
//! plans it. A run compacts before a model call once the conversation passes
//! the threshold, or when the user or the model asks; a thread that is not
//! answering is compacted on its own run.

mod hook;
mod tool;

use std::sync::Arc;

use anyhow::{Context, Result};
use rig_core::catalog::ModelSpec;
use rig_core::message::Message;
use rig_memory::{CompactReason, CompactionPolicy, Summarizer, SummaryState, TrackArgument, estimate_tokens};

use crate::agent::Model;
use crate::config::{self, Providers};
use crate::protocol::Event;

pub use hook::{Applied, Hook, Live};
pub use tool::Compact;

pub const TOOL: &str = "compact";

/// The files a summary keeps track of across compactions; one changed is
/// listed as such rather than as read.
const TRACKED: [TrackArgument; 3] = [
    TrackArgument::new("read", "path", "files-read"),
    TrackArgument::new("write", "path", "files-modified"),
    TrackArgument::new("edit", "path", "files-modified"),
];

/// How rig-memory opens the message that stands for the messages a summary
/// replaced ([`SummaryState::message`]), by which a saved conversation's
/// summary is told from what the user typed.
const SUMMARY_OPEN: &str = "The conversation before this point was compacted into this summary:\n\n<summary>\n";
const SUMMARY_CLOSE: &str = "\n</summary>";

/// The summary `text` holds, when it is the message that stands for the
/// messages a compaction replaced.
pub fn summary_in(text: &str) -> Option<&str> {
    text.strip_prefix(SUMMARY_OPEN)?
        .split_once(SUMMARY_CLOSE)
        .map(|(summary, _)| summary)
}

/// Writes summaries of a conversation for its model, and says when it is
/// due for one.
pub struct Compactor {
    summarizer: Model,
    /// What the summarizer reads and writes in one request.
    summarizer_spec: ModelSpec,
    /// The conversation's model, whose window bounds what is kept.
    spec: ModelSpec,
    policy: CompactionPolicy,
    /// Tokens past which a model call compacts first; `None` when only
    /// asking compacts.
    threshold: Option<u64>,
}

/// A summary of a conversation's messages before its `upto`th.
pub struct Summary {
    pub upto: usize,
    /// The summary, with what it keeps track of.
    pub state: SummaryState,
    /// Estimated tokens of the messages it replaces.
    pub before: u64,
}

impl Summary {
    /// The message that stands for the messages it replaces.
    pub fn message(&self) -> Message {
        Message::user(self.state.message().unwrap_or_default())
    }

    /// What clients are told of it.
    pub fn event(&self) -> Event {
        Event::Compacted {
            summary: self.state.summary.clone(),
            before: self.before,
            after: estimate_tokens(&[self.message()]),
        }
    }
}

impl Compactor {
    /// For a conversation with `model`, its summaries written by the model
    /// `config` names, else by `model`.
    pub fn new(config: &config::Compaction, model: &Model, providers: &Providers) -> Result<Self> {
        let spec = model.spec()?;
        let summarizer = match &config.model {
            Some(reference) => Model::connect(reference, providers).context("cannot use `compaction.model`")?,
            None => model.clone(),
        };
        let threshold = spec
            .input_limit()
            .map_or(config.threshold, |window| config.threshold.min(u64::from(window) / 10 * 9));
        let defaults = CompactionPolicy::default();
        // What is kept as it is stays well under the threshold, so the
        // conversation is not due again at once.
        let keep_recent = usize::try_from(threshold / 4).unwrap_or(usize::MAX);
        let policy = CompactionPolicy {
            tracked: TRACKED.to_vec(),
            keep_recent: defaults.keep_recent.min(keep_recent),
            ..defaults
        };
        Ok(Self {
            summarizer_spec: summarizer.spec()?,
            summarizer,
            spec,
            policy,
            threshold: config.auto.then_some(threshold),
        })
    }

    /// Whether a model call of about `tokens` should compact first.
    pub fn due(&self, tokens: u64) -> bool {
        self.threshold.is_some_and(|threshold| tokens > threshold)
    }

    /// A summary for `reason` of the older of `messages`, whose first
    /// `from` are summarized in `state` already; `None` when nothing new
    /// would be summarized.
    pub async fn summarize(
        &self,
        messages: &[Arc<Message>],
        from: usize,
        state: &SummaryState,
        reason: &CompactReason,
    ) -> Result<Option<Summary>> {
        let Some(upto) = self.policy.cut(messages, from, reason, Some(&self.spec)) else {
            return Ok(None);
        };
        let (mut state, request) = self
            .policy
            .plan(state, messages, from, upto, &self.summarizer_spec, reason)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let response = self.summarizer.completion().call(request).await?;
        state.summary = Summarizer::summary_text(&response)?;
        Ok(Some(Summary {
            upto,
            state,
            before: estimate_tokens(messages.get(..upto).unwrap_or_default()),
        }))
    }
}

/// Compactions asked of a run, by the user or the model, for its next model
/// call to make; when several are asked, their focuses are joined.
#[derive(Debug)]
pub enum Requests {
    Open(Option<String>),
    /// The run makes no more model calls.
    Closed,
}

impl Default for Requests {
    fn default() -> Self {
        Self::Open(None)
    }
}

impl Requests {
    /// False once the run takes no more.
    pub fn push(&mut self, focus: &str) -> bool {
        let Self::Open(asked) = self else {
            return false;
        };
        let focuses = asked.take().into_iter().chain(Some(focus.trim().to_owned()));
        let focuses: Vec<String> = focuses.filter(|focus| !focus.is_empty()).collect();
        *asked = Some(focuses.join("\n"));
        true
    }

    pub fn take(&mut self) -> Option<String> {
        match self {
            Self::Open(asked) => asked.take(),
            Self::Closed => None,
        }
    }

    /// Takes no more, returning what was asked and not yet made.
    pub fn close(&mut self) -> Option<String> {
        let asked = self.take();
        *self = Self::Closed;
        asked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_summary_is_told_from_what_the_user_typed() {
        let state = SummaryState {
            summary: "## Goal\nShip it".into(),
            tracked: vec![rig_memory::TrackedSet {
                name: "files-read".into(),
                values: ["src/main.rs".to_owned()].into(),
            }],
        };
        let message = state.message().unwrap();
        assert_eq!(summary_in(&message), Some("## Goal\nShip it"));
        assert_eq!(summary_in("## Goal\nShip it"), None);
    }

    #[test]
    fn asked_focuses_join_until_the_run_takes_no_more() {
        let mut requests = Requests::default();
        assert!(requests.push(""));
        assert!(requests.push(" the parser "));
        assert!(requests.push("its tests"));
        assert_eq!(requests.take().as_deref(), Some("the parser\nits tests"));
        assert_eq!(requests.take(), None);
        assert!(requests.push(""));
        assert_eq!(requests.close().as_deref(), Some(""));
        assert!(!requests.push("late"));
    }
}
