/* -------------------------------------------------------------------------- */
/*                                  tools.rs                                  */
/* -------------------------------------------------------------------------- */

//! Tools that work on the conversation rather than the machine: the agent's
//! todo list, questions to the user, and the plan that ends plan mode.

use std::sync::Arc;

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::host::Run;
use crate::notes::{Notes, Todo, TodoStatus};
use crate::protocol::{Ask, Decision, Event};

pub const TODO: &str = "todo";
pub const ASK_USER: &str = "ask_user";
pub const EXIT_PLAN: &str = "exit_plan";

/// What the model is told with a message sent in plan mode.
pub const PLAN_REMINDER: &str = "<plan_mode>\nThis conversation is in plan mode: only tools \
that change nothing run. Investigate, then present your plan with `exit_plan`; the user approves \
it before you change anything.\n</plan_mode>";

/// Whether `text` is the part of a user message that reminds the model of
/// plan mode, rather than what the user typed.
pub fn is_reminder(text: &str) -> bool {
    text.starts_with("<plan_mode>")
}

const TODO_GUIDANCE: &str = "Writes your todo list for this conversation, replacing the one \
before: every item, each `pending`, `in_progress` or `done`. Use it for work of several steps, \
to plan them and show the user your progress: mark an item `in_progress` as you start it and \
`done` as soon as it is, keeping one `in_progress` at a time. The list outlasts compaction. \
Skip it for a task of one or two steps.";

const ASK_GUIDANCE: &str = "Asks the user a question and waits for the answer. Use it when you \
cannot go on well without their decision, such as between approaches that differ in what they \
change, or when the request is ambiguous; not for what you can find out with your tools. Offer \
`choices` when the answers are few; the user can always type another.";

const PLAN_GUIDANCE: &str = "In plan mode, presents your plan to the user for approval. Call it \
once you have investigated enough to plan the work: the plan, in Markdown, says what you will \
change, where and how, in steps. If the user approves, plan mode ends and you carry it out; \
otherwise you are told what they want changed, and go on planning.";

/// The tool that writes the agent's todo list.
pub struct Todos {
    pub notes: Arc<Notes>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoArgs {
    /// The whole list, in order.
    pub todos: Vec<Todo>,
}

impl Tool for Todos {
    const NAME: &'static str = TODO;
    type Args = TodoArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        TODO_GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<TodoArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_open_world(false)
    }

    async fn call(&self, _context: &mut ToolContext, args: TodoArgs) -> Result<String, ToolExecutionError> {
        let done = args.todos.iter().filter(|todo| todo.status == TodoStatus::Done).count();
        let total = args.todos.len();
        self.notes.set_todos(args.todos);
        Ok(format!("{done} of {total} done"))
    }
}

/// The tool that asks the user a question.
pub struct AskUser {
    pub run: Arc<Run>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    /// The question, in one or two sentences.
    pub question: String,
    /// Answers to pick from, if they are few; leave it out for an open question.
    #[serde(default)]
    pub choices: Vec<String>,
}

impl Tool for AskUser {
    const NAME: &'static str = ASK_USER;
    type Args = AskArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        ASK_GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<AskArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_read_only(true).with_open_world(false)
    }

    async fn call(&self, _context: &mut ToolContext, args: AskArgs) -> Result<String, ToolExecutionError> {
        let ask = Ask::Question {
            question: args.question,
            choices: args.choices,
        };
        match self.run.ask(ask).await {
            Decision::Answer(answer) => Ok(answer),
            Decision::Stop => Err(stopped(&self.run)),
            Decision::Once | Decision::Always | Decision::Decline => Err(ToolExecutionError::new(
                ToolErrorKind::Other,
                "the user dismissed the question without answering; go on as you judge best, or ask otherwise",
            )),
        }
    }
}

/// The tool that puts the agent's plan to the user, ending plan mode once
/// it is approved.
pub struct ExitPlan {
    pub run: Arc<Run>,
    pub notes: Arc<Notes>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanArgs {
    /// The plan, in Markdown.
    pub plan: String,
}

impl Tool for ExitPlan {
    const NAME: &'static str = EXIT_PLAN;
    type Args = PlanArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        PLAN_GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<PlanArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_open_world(false)
    }

    async fn call(&self, _context: &mut ToolContext, args: PlanArgs) -> Result<String, ToolExecutionError> {
        if !self.notes.planning() {
            return Err(ToolExecutionError::new(
                ToolErrorKind::Other,
                "this conversation is not in plan mode; go on with the work",
            ));
        }
        match self.run.ask(Ask::Plan { plan: args.plan }).await {
            Decision::Once | Decision::Always => {
                self.notes.set_planning(false);
                self.run.emit(Event::Plan { on: false });
                Ok("The user approved the plan, and plan mode is over: carry it out.".into())
            }
            Decision::Answer(feedback) => Ok(format!(
                "The user did not approve the plan yet; still in plan mode. Their feedback: {feedback}"
            )),
            Decision::Decline => Ok("The user did not approve the plan; still in plan mode. Ask \
                what they want changed, or revise it."
                .into()),
            Decision::Stop => Err(stopped(&self.run)),
        }
    }
}

fn stopped(run: &Run) -> ToolExecutionError {
    run.stop();
    ToolExecutionError::new(ToolErrorKind::Other, "The user stopped the run.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Permissions;

    fn context() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn an_approved_plan_ends_plan_mode_and_feedback_keeps_it() {
        let run = Arc::new(Run::new("hi".into(), "test/model".into(), 0, || {}));
        let notes = Arc::new(Notes::load(None, Arc::new(Permissions::default())));
        notes.set_planning(true);
        let tool = Arc::new(ExitPlan {
            run: run.clone(),
            notes: notes.clone(),
        });
        let args = || PlanArgs { plan: "1. edit".into() };

        let asked = tokio::spawn({
            let tool = tool.clone();
            async move { tool.call(&mut context(), args()).await }
        });
        while !run.asking() {
            tokio::task::yield_now().await;
        }
        assert!(run.answer(crate::protocol::ApprovalId(0), Decision::Answer("smaller".into())));
        let told = asked.await.unwrap().unwrap();
        assert!(told.contains("smaller") && notes.planning(), "{told}");

        let asked = tokio::spawn({
            let tool = tool.clone();
            async move { tool.call(&mut context(), args()).await }
        });
        while !run.asking() {
            tokio::task::yield_now().await;
        }
        assert!(run.answer(crate::protocol::ApprovalId(1), Decision::Once));
        asked.await.unwrap().unwrap();
        assert!(!notes.planning());
    }
}
