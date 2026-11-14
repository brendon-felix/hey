/* -------------------------------------------------------------------------- */
/*                             compaction/hook.rs                             */
/* -------------------------------------------------------------------------- */

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, HookContext, ModelTurnAction, ModelTurnFinished,
};
use rig_agent::run::RequestPatch;
use rig_core::message::Message;
use rig_memory::{CompactReason, SummaryState, estimate_tokens};

use super::Compactor;
use crate::host::Run;
use crate::protocol::Event;

/// A run's compaction. The agent loads the saved conversation as each of
/// its streams starts and saves all it said as each ends, so a summary
/// made in between stands in for the older messages only in what model
/// calls are sent; the host saves it in their place once the run ends.
/// Positions are in the conversation as the run has it, uncompacted.
pub struct Live {
    compactor: Compactor,
    run: Arc<Run>,
    state: Mutex<State>,
}

struct State {
    /// How many of the saved conversation's first messages are a summary
    /// already, and that summary.
    saved: (usize, SummaryState),
    applied: Option<Applied>,
    /// What the last model call was reported to take in and give back.
    anchor: Option<Anchor>,
    /// How many messages the last model call was sent, its prompt included.
    sent: usize,
    /// A compaction the threshold called for failed: the run goes on
    /// without, rather than fail at every call.
    stalled: bool,
}

struct Anchor {
    tokens: u64,
    /// How many messages of the conversation it counts.
    through: usize,
}

/// A summary in place of the conversation's messages before its `at`th.
#[derive(Clone)]
pub struct Applied {
    pub at: usize,
    /// The message at `at`, by which it is found where the conversation
    /// is saved.
    pub kept: Arc<Message>,
    pub state: SummaryState,
    message: Arc<Message>,
}

impl Live {
    /// For a run on a saved conversation of `history` messages, whose first
    /// `from` are summarized in `state`, and whose last model call took in
    /// and gave back `context` tokens, when that is known.
    pub fn new(compactor: Compactor, run: Arc<Run>, history: usize, from: usize, state: SummaryState, context: Option<u64>) -> Self {
        Self {
            compactor,
            run,
            state: Mutex::new(State {
                saved: (from, state),
                applied: None,
                anchor: context.map(|tokens| Anchor { tokens, through: history }),
                sent: 0,
                stalled: false,
            }),
        }
    }

    pub fn compactor(&self) -> &Compactor {
        &self.compactor
    }

    /// The summary made, for the host to save.
    pub fn applied(&self) -> Option<Applied> {
        self.lock().applied.clone()
    }

    /// What the last model call was reported to take in and give back.
    pub fn context(&self) -> Option<u64> {
        self.lock().anchor.as_ref().map(|anchor| anchor.tokens)
    }

    /// The history a model call on `conversation`, its prompt last, is sent
    /// instead, compacting first when asked or due; `None` to send it as
    /// it is.
    async fn prepare(&self, conversation: &[Arc<Message>]) -> Option<Vec<Arc<Message>>> {
        let (view, from, summarized, reason) = {
            let mut state = self.lock();
            state.sent = conversation.len();
            let view = state.view(conversation);
            let asked = self.run.take_compaction().map(|focus| CompactReason::Asked { focus });
            let due = !state.stalled && self.compactor.due(state.estimate(conversation, &view));
            match asked.or(due.then_some(CompactReason::Threshold)) {
                None => return state.history(view),
                Some(reason) => {
                    let (from, summarized) = state.summarized();
                    (view, from, summarized.clone(), reason)
                }
            }
        };
        self.run.emit(Event::Notice {
            text: "compacting the conversation…".into(),
        });
        let made = self.compactor.summarize(&view, from, &summarized, &reason).await;
        let mut state = self.lock();
        match made {
            Ok(Some(summary)) => {
                // Past the summary standing in for them, the view holds the
                // conversation's messages from those it kept.
                let at = state
                    .applied
                    .as_ref()
                    .map_or(summary.upto, |applied| applied.at + summary.upto - 1);
                tracing::info!(run = ?self.run.id(), at, tokens = summary.before, "compacted");
                self.run.emit(summary.event());
                state.applied = conversation.get(at).map(|kept| Applied {
                    at,
                    kept: kept.clone(),
                    message: Arc::new(summary.message()),
                    state: summary.state,
                });
                state.anchor = None;
                let view = state.view(conversation);
                state.history(view)
            }
            Ok(None) => {
                if let CompactReason::Asked { .. } = reason {
                    self.run.emit(Event::Notice {
                        text: "nothing to compact yet".into(),
                    });
                }
                state.history(view)
            }
            Err(error) => {
                tracing::warn!(run = ?self.run.id(), error = format!("{error:#}"), "cannot compact");
                self.run.emit(Event::Notice {
                    text: format!("not compacted: {error:#}"),
                });
                state.stalled |= reason == CompactReason::Threshold;
                state.history(view)
            }
        }
    }

    fn reported(&self, tokens: Option<u64>) {
        let mut state = self.lock();
        // The call's reply follows what it was sent.
        let through = state.sent + 1;
        state.anchor = tokens.map(|tokens| Anchor { tokens, through });
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// What a model call on `conversation` reads of it: its newest messages
    /// after the summary standing in for the rest, if one was made.
    fn view(&self, conversation: &[Arc<Message>]) -> Vec<Arc<Message>> {
        match &self.applied {
            Some(applied) => std::iter::once(applied.message.clone())
                .chain(conversation.get(applied.at..).unwrap_or_default().iter().cloned())
                .collect(),
            None => conversation.to_vec(),
        }
    }

    /// How many first messages of the view are a summary, and its state.
    fn summarized(&self) -> (usize, &SummaryState) {
        match &self.applied {
            Some(applied) => (1, &applied.state),
            None => (self.saved.0, &self.saved.1),
        }
    }

    /// About how many tokens a call on `view` of `conversation` takes in:
    /// what the last was reported to, and the messages since, else the
    /// whole view.
    fn estimate(&self, conversation: &[Arc<Message>], view: &[Arc<Message>]) -> u64 {
        let anchored = self.anchor.as_ref().and_then(|anchor| {
            let since = conversation.get(anchor.through..)?;
            Some(anchor.tokens + estimate_tokens(since))
        });
        anchored.unwrap_or_else(|| estimate_tokens(view))
    }

    /// `view` without its prompt, as a call is sent it in place of its
    /// history, when a summary stands in for some of it.
    fn history(&self, mut view: Vec<Arc<Message>>) -> Option<Vec<Arc<Message>>> {
        self.applied.as_ref()?;
        view.pop();
        Some(view)
    }
}

pub struct Hook(pub Arc<Live>);

impl AgentHook for Hook {
    async fn on_completion_call(&self, _ctx: &HookContext, event: CompletionCallEvent<'_>) -> CompletionCallAction {
        let conversation: Vec<Arc<Message>> = event
            .history
            .iter()
            .cloned()
            .chain([Arc::new(event.prompt.clone())])
            .collect();
        match self.0.prepare(&conversation).await {
            Some(history) => CompletionCallAction::patch(RequestPatch {
                history: Some(history),
                ..RequestPatch::default()
            }),
            None => CompletionCallAction::continue_run(),
        }
    }

    async fn on_model_turn_finished(&self, _ctx: &HookContext, event: ModelTurnFinished<'_>) -> ModelTurnAction {
        self.0.reported(event.usage.context_tokens());
        ModelTurnAction::continue_run()
    }
}

#[cfg(test)]
mod tests {
    use rig_agent::agent::AgentBuilder;
    use rig_core::completion::{CompletionRequest, Usage};
    use rig_core::message::UserContent;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    use super::*;
    use crate::agent::Model;
    use crate::compaction::summary_in;
    use crate::config::{self, Providers};
    use crate::skills::Load;

    fn texts(request: &CompletionRequest) -> Vec<String> {
        let texts = request.chat_history.iter().flat_map(|message| match &**message {
            Message::User { content } => content
                .iter()
                .filter_map(|content| match content {
                    UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        });
        texts.collect()
    }

    #[tokio::test]
    async fn a_call_past_the_threshold_is_sent_a_summary_in_place_of_older_messages() {
        let mut usage = Usage::default();
        usage.total_tokens = Some(50_000);
        let answering = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "skill", serde_json::json!({ "name": "none" })).with_usage(usage),
            MockTurn::text("done"),
        ]);
        let summarizing = MockCompletionModel::text("Asked about the parser.");
        let config = config::Compaction {
            auto: true,
            threshold: 10_000,
            model: None,
        };
        let summarizer = Model::scripted("openai/mock", summarizing.clone());
        let compactor = Compactor::new(&config, &summarizer, &Providers::default()).unwrap();
        let run = Arc::new(Run::new("q3".into(), "openai/mock".into(), 4, || {}));
        let live = Arc::new(Live::new(compactor, run, 4, 0, SummaryState::default(), None));
        let history = [
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2 ".repeat(4_000)),
        ];

        AgentBuilder::new(answering.clone())
            .tool(Load { skills: Arc::from([]) })
            .add_hook(Hook(live.clone()))
            .default_max_turns(3)
            .build()
            .prompt("q3")
            .history(history)
            .run()
            .await
            .unwrap();

        let sent = answering.requests();
        assert_eq!(sent.len(), 2);
        assert_eq!(texts(&sent[0]).first().map(String::as_str), Some("q1"), "not due before the first call");
        let after = texts(&sent[1]);
        assert_eq!(summary_in(&after[0]), Some("Asked about the parser."));
        assert!(!after.iter().any(|text| text == "q1" || text == "q2"), "{after:?}");
        assert_eq!(summarizing.request_count(), 1);
        assert_eq!(live.applied().map(|applied| applied.at), Some(3), "kept from the long answer");
    }
}
