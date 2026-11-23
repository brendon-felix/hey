/* -------------------------------------------------------------------------- */
/*                                 budget.rs                                  */
/* -------------------------------------------------------------------------- */

//! The `max_turns` limit, told to the model before it is reached. An answer
//! that hits it is cut off with an error, so the model is told as it nears
//! the limit to wrap up or ask the user whether to go on, and on its last
//! call it may not use tools, so that it ends with words the user can read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rig_agent::agent::{AgentHook, CompletionCallAction, CompletionCallEvent, HookContext};
use rig_agent::run::RequestPatch;
use rig_core::completion::Document;
use rig_core::message::ToolChoice;

use crate::host::Run;
use crate::protocol::Event;

/// Model calls before the last at which the warning begins.
const WARN_WITHIN: usize = 5;

pub struct Budget {
    pub max_turns: usize,
    /// The run to tell the user on, when there is one.
    pub run: Option<Arc<Run>>,
    told: AtomicBool,
}

impl Budget {
    pub fn new(max_turns: usize, run: Option<Arc<Run>>) -> Self {
        Self {
            max_turns,
            run,
            told: AtomicBool::new(false),
        }
    }

    /// What the model is told on its `turn`th call (one-based), if anything,
    /// and whether it is its last.
    fn note(&self, turn: usize) -> Option<(String, bool)> {
        let left = self.max_turns.checked_sub(turn)?;
        // Short limits warn from the start only on the last call.
        let warn_within = WARN_WITHIN.min(self.max_turns / 2);
        match left {
            0 => Some((
                format!(
                    "This is your last model call: the limit of {} was reached. You cannot \
                     use tools now. Reply with what you have done so far and what is left, \
                     and ask the user whether you should keep going.",
                    self.max_turns
                ),
                true,
            )),
            left if left <= warn_within => Some((
                format!(
                    "You are nearing the limit of {} model calls per reply: {left} left \
                     after this one. Finish up now: wrap up your reply, or stop and check \
                     with the user, with `ask_user` or in your reply, whether you should \
                     keep going. Do not start new work.",
                    self.max_turns
                ),
                false,
            )),
            _ => None,
        }
    }
}

impl AgentHook for Budget {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let Some((note, last)) = self.note(event.turn) else {
            return CompletionCallAction::continue_run();
        };
        if !self.told.swap(true, Ordering::Relaxed)
            && let Some(run) = &self.run
        {
            run.emit(Event::Notice {
                text: format!(
                    "nearing max_turns ({}); asking the agent to wrap up",
                    self.max_turns
                ),
            });
        }
        let mut patch = RequestPatch::default().extra_context([Document {
            id: "turn_limit".into(),
            text: note,
            additional_props: Default::default(),
        }]);
        if last {
            patch = patch.tool_choice(ToolChoice::None);
        }
        CompletionCallAction::patch(patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_near_the_limit_and_forbids_tools_on_the_last_call() {
        let budget = Budget::new(10, None);
        assert!(budget.note(4).is_none());
        let (warning, last) = budget.note(5).unwrap();
        assert!(warning.contains("nearing") && !last);
        assert!(budget.note(9).is_some_and(|(_, last)| !last));
        assert!(budget.note(10).is_some_and(|(_, last)| last));
        assert!(Budget::new(2, None).note(1).is_some());
    }
}
