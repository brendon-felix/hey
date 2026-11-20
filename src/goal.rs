/* -------------------------------------------------------------------------- */
/*                                  goal.rs                                   */
/* -------------------------------------------------------------------------- */

//! `/goal`: a condition the agent works toward. After each answer, a model
//! judges from the latest reply whether it is met; until it is, the agent
//! is sent back to work with the judge's reason.

use anyhow::Result;
use rig_agent::agent::AgentBuilder;
use serde::Deserialize;

use crate::agent::Model;

const PROMPT: &str = "You judge whether an AI agent has met a goal, from its latest reply alone. \
Answer only with JSON: {\"met\": true or false, \"reason\": \"...\"}. When it is not met, the \
reason says, in a sentence or two addressed to the agent, what is still missing.";

/// What the judge decided.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Judgement {
    pub met: bool,
    #[serde(default)]
    pub reason: String,
}

/// Whether `reply` shows `condition` met, as `model` judges it.
pub async fn judge(model: &Model, condition: &str, reply: &str) -> Result<Judgement> {
    let judge = AgentBuilder::new(model.completion())
        .preamble(PROMPT)
        .build();
    let request =
        format!("<goal>\n{condition}\n</goal>\n\n<latest_reply>\n{reply}\n</latest_reply>");
    let answer = crate::retry::model("judging a goal", || async {
        judge.prompt(request.clone()).await
    })
    .await?
    .output();
    read(&answer)
}

/// The judgement in `answer`, which may wrap its JSON in prose or a fence.
fn read(answer: &str) -> Result<Judgement> {
    let start = answer.find('{');
    let end = answer.rfind('}');
    match (start, end) {
        (Some(start), Some(end)) if start < end => Ok(serde_json::from_str(&answer[start..=end])?),
        _ => anyhow::bail!("the judge did not answer with JSON: {answer}"),
    }
}

/// What the agent is told when its goal is not met.
pub fn prompt(condition: &str, reason: &str) -> String {
    format!("The goal is not met yet: {condition}\n\n{reason}\n\nKeep working until it is.")
}

#[cfg(test)]
mod tests {
    use rig_core::test_utils::MockCompletionModel;

    use super::*;

    #[test]
    fn reads_json_wrapped_in_prose() {
        let judged =
            read("Sure:\n```json\n{\"met\": false, \"reason\": \"tests fail\"}\n```").unwrap();
        assert_eq!(
            judged,
            Judgement {
                met: false,
                reason: "tests fail".into()
            }
        );
        assert!(read("yes").is_err());
    }

    #[tokio::test]
    async fn the_judge_reads_the_goal_and_reply() {
        let mock = MockCompletionModel::text("{\"met\": true}");
        let model = Model::scripted("openai/mock", mock.clone());
        let judged = judge(&model, "all tests pass", "All 40 tests pass.")
            .await
            .unwrap();
        assert!(judged.met);
        assert_eq!(mock.request_count(), 1);
    }
}
