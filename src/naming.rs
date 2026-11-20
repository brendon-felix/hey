/* -------------------------------------------------------------------------- */
/*                                 naming.rs                                  */
/* -------------------------------------------------------------------------- */

//! Thread names: after an answer, a model names the thread from what was
//! said, and keeps the name it gave until the topic has shifted enough.

use anyhow::Result;
use rig_agent::agent::AgentBuilder;
use rig_core::message::Message;
use serde::Deserialize;

use crate::agent::Model;
use crate::transcript;

const PROMPT: &str = "You name conversations between a user and an AI coding agent, so the user \
can tell them apart in a list. A good name is 3 to 8 words, in the language the user writes in, \
says what the conversation is about and what it is for, and has no quotes or trailing period. \
You are given the current name, if any, and an excerpt of the conversation. Keep the current \
name unless the conversation has clearly moved to a different topic or purpose, or the name \
is vague or misleading; small follow-ups, details and fixes do not change the topic. \
Answer only with JSON: {\"title\": \"...\"} with the name to use (the current one to keep it).";

const HEAD: usize = 1500;
const TAIL: usize = 3000;
const MAX_TITLE: usize = 80;

#[derive(Deserialize)]
struct Named {
    title: String,
}

/// The name `model` gives the conversation `messages`, which is `current`
/// when the topic has not shifted enough. None when nothing was said yet.
pub async fn name(
    model: &Model,
    current: Option<&str>,
    messages: &[Message],
) -> Result<Option<String>> {
    let said = transcript::thread(messages);
    if said.is_empty() {
        return Ok(None);
    }
    let namer = AgentBuilder::new(model.completion())
        .preamble(PROMPT)
        .build();
    let request = format!(
        "<current_name>\n{}\n</current_name>\n\n<conversation>\n{}\n</conversation>",
        current.unwrap_or("(none yet)"),
        excerpt(&said)
    );
    let answer = crate::retry::model("naming a thread", || async {
        namer.prompt(request.clone()).await
    })
    .await?
    .output();
    Ok(read(&answer))
}

/// The start and the end of `text`, as much as is worth reading.
fn excerpt(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= HEAD + TAIL {
        return text.to_owned();
    }
    let head: String = chars[..HEAD].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}\n\n[…]\n\n{tail}")
}

/// The title in `answer`, which may wrap its JSON in prose or a fence.
fn read(answer: &str) -> Option<String> {
    let (start, end) = (answer.find('{')?, answer.rfind('}')?);
    let named: Named = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let title = named.title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title
        .trim_matches(['"', '“', '”', '\'', '.'])
        .chars()
        .take(MAX_TITLE)
        .collect::<String>();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use rig_core::test_utils::MockCompletionModel;

    use super::*;

    #[test]
    fn reads_a_title_wrapped_in_prose() {
        assert_eq!(
            read("```json\n{\"title\": \"  Fix the “login” bug. \"}\n```").as_deref(),
            Some("Fix the “login” bug")
        );
        assert_eq!(read("no"), None);
        assert_eq!(read("{\"title\": \"\"}"), None);
    }

    #[tokio::test]
    async fn names_a_conversation_from_what_was_said() {
        let mock = MockCompletionModel::text("{\"title\": \"Rename threads with an LLM\"}");
        let model = Model::scripted("openai/mock", mock.clone());
        let messages = [
            Message::user("name threads better"),
            Message::assistant("ok"),
        ];
        let named = name(&model, None, &messages).await.unwrap();
        assert_eq!(named.as_deref(), Some("Rename threads with an LLM"));
        assert_eq!(name(&model, None, &[]).await.unwrap(), None);
        assert_eq!(mock.request_count(), 1);
    }
}
