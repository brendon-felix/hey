/* -------------------------------------------------------------------------- */
/*                               host/record.rs                               */
/* -------------------------------------------------------------------------- */

//! What a run has said so far. Rig saves a run's exchange only once it
//! finishes, so a run that is stopped or fails leaves this to be saved
//! instead, with as much of the reply in progress as had arrived.

use std::sync::{Arc, Mutex, PoisonError};

use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, HookContext, ModelTurnAction, ModelTurnFinished,
    ObservationAction, RunStart, RunStartAction, TextDelta,
};
use rig_core::conversation::pairing;
use rig_core::message::{AssistantContent, AssistantMessage, Message};

/// A hook keeping what its run said; clones share it.
#[derive(Clone, Default)]
pub struct Record(Arc<Mutex<Said>>);

#[derive(Default)]
struct Said {
    /// How many messages the conversation held before the run.
    before: usize,
    /// The conversation as the model call in progress was sent it.
    sent: Vec<Message>,
    reply: Reply,
}

/// The reply of the model call in progress.
#[derive(Default)]
enum Reply {
    #[default]
    Waiting,
    /// The text streamed so far.
    Streaming(String),
    /// The whole reply, before its tool calls ran.
    Finished(Vec<AssistantContent>),
}

impl Record {
    /// What the run said, its unanswered tool calls closed: its prompt,
    /// the turns since, and the reply in progress, as one ended for
    /// `reason`. Reasoning and tool calls still streaming are left out, as
    /// they mean nothing cut short.
    pub fn said(&self, reason: &str) -> Vec<Message> {
        let said = self.lock();
        let reply = match &said.reply {
            Reply::Waiting => None,
            Reply::Streaming(text) if text.trim().is_empty() => None,
            Reply::Streaming(text) => Some(AssistantMessage::aborted(None, vec![AssistantContent::text(text)], reason)),
            Reply::Finished(content) => Some(AssistantMessage::new(content.clone())),
        };
        let messages = said
            .sent
            .iter()
            .skip(said.before)
            .cloned()
            .chain(reply.map(Message::Assistant))
            .collect();
        pairing::repair(messages).messages
    }

    /// How many messages the conversation held as the run's latest answer
    /// began: more than as the run began once it answers what was sent to
    /// it meanwhile.
    pub fn held(&self) -> usize {
        self.lock().before
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Said> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AgentHook for Record {
    async fn on_run_start(&self, _ctx: &HookContext, event: RunStart<'_>) -> RunStartAction {
        let mut said = self.lock();
        said.before = event.history.len();
        said.sent = sent(event.history, event.prompt);
        RunStartAction::Continue
    }

    async fn on_completion_call(&self, _ctx: &HookContext, event: CompletionCallEvent<'_>) -> CompletionCallAction {
        let mut said = self.lock();
        said.sent = sent(event.history, event.prompt);
        said.reply = Reply::Waiting;
        CompletionCallAction::Continue
    }

    async fn on_text_delta(&self, _ctx: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        let mut said = self.lock();
        match &mut said.reply {
            Reply::Streaming(text) => text.push_str(event.delta),
            reply => *reply = Reply::Streaming(event.delta.to_owned()),
        }
        ObservationAction::Continue
    }

    async fn on_model_turn_finished(&self, _ctx: &HookContext, event: ModelTurnFinished<'_>) -> ModelTurnAction {
        self.lock().reply = Reply::Finished(event.content.clone());
        ModelTurnAction::Continue
    }
}

fn sent(history: &[Arc<Message>], prompt: &Message) -> Vec<Message> {
    history
        .iter()
        .map(|message| Message::clone(message))
        .chain([prompt.clone()])
        .collect()
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use rig_agent::agent::AgentBuilder;
    use rig_core::completion::Usage;
    use rig_core::message::{StopReason, ToolName};
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig_core::tool::{DynamicTool, ToolOutput};
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn a_reply_cut_short_keeps_what_the_run_said_so_far() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::text("Two down."),
                MockStreamEvent::tool_call("1", "note", json!({})),
                MockStreamEvent::final_response(Usage::default()),
            ],
            vec![MockStreamEvent::text("Now 08 and 09:"), MockStreamEvent::error("connection reset")],
        ]);
        let note = DynamicTool::new(ToolName::new("note").unwrap(), "", json!({ "type": "object" }), |_| {
            Box::pin(async { Ok(ToolOutput::text("noted")) })
        });
        let record = Record::default();
        let agent = AgentBuilder::new(model)
            .dynamic_tools(vec![note])
            .default_max_turns(5)
            .add_hook(record.clone())
            .build();
        let items: Vec<_> = agent.prompt("write the documents").stream().collect().await;
        assert!(items.last().is_some_and(Result::is_err), "the run fails");

        let said = record.said("connection reset");
        assert!(pairing::validate(&said).is_ok());
        assert_eq!(said.len(), 4, "the prompt, the first turn, its result and the reply cut short");
        assert!(matches!(&said[0], Message::User { .. }));
        let Message::Assistant(cut) = &said[3] else {
            panic!("{:?}", said[3]);
        };
        assert!(matches!(&cut.stop, Some(StopReason::Aborted(reason)) if reason == "connection reset"));
        assert!(matches!(&cut.content[..], [AssistantContent::Text(text)] if text.text == "Now 08 and 09:"));
    }
}
