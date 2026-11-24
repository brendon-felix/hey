/* -------------------------------------------------------------------------- */
/*                                 recovery.rs                                */
/* -------------------------------------------------------------------------- */

//! A call to a tool the model may not use ends the reply with an error by
//! default. This answers it as a failed tool call instead, telling the model
//! what went wrong, so that it goes on, tries another way or asks the user.

use rig_agent::agent::{AgentHook, HookContext};
use rig_agent::run::policy::{InvalidToolCallAction, InvalidToolCallContext, InvalidToolCallReason};

pub struct Recovery;

/// What the model is told about `call`, which cannot be run as written.
fn feedback(call: &InvalidToolCallContext) -> String {
    let name = &call.tool_name;
    let problem = match &call.reason {
        InvalidToolCallReason::UnknownTool => {
            format!("`{name}` is not a tool you have")
        }
        InvalidToolCallReason::DisallowedByToolChoice => {
            format!("`{name}` may not be used in this reply")
        }
        InvalidToolCallReason::MalformedArguments { error } => {
            format!("the arguments for `{name}` are not a JSON object ({error})")
        }
        _ => format!("the call to `{name}` cannot be run"),
    };
    let usable = if call.allowed_tools.is_empty() {
        "You cannot use any tools in this reply.".to_owned()
    } else {
        format!("The tools you can use are: {}.", call.allowed_tools.join(", "))
    };
    format!(
        "The call failed and nothing ran: {problem}. {usable} Go on with another way, or if \
         you cannot, tell the user what stopped you and ask how to proceed."
    )
}

impl AgentHook for Recovery {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        // A reply that may use no tools has nowhere to put a tool result.
        if event.allowed_tools.is_empty() {
            return None;
        }
        Some(InvalidToolCallAction::skip(feedback(event)))
    }
}

#[cfg(test)]
mod tests {
    use rig_agent::agent::AgentBuilder;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    use super::*;

    #[tokio::test]
    async fn a_call_to_a_tool_it_does_not_have_is_answered_and_the_reply_goes_on() {
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "nope", serde_json::json!({})),
            MockTurn::text("tried another way"),
        ]);
        let reply = AgentBuilder::new(model.clone())
            .tool(crate::skills::Load {
                skills: std::sync::Arc::from([]),
            })
            .add_hook(Recovery)
            .default_max_turns(3)
            .build()
            .prompt("go")
            .run()
            .await;
        assert!(reply.is_ok(), "{reply:?}");
        assert_eq!(model.requests().len(), 2);
    }
}
