use async_openai::types::{
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestUserMessageContent,
};
use anyhow::Result;

use yansi::Paint;

use crate::{
    render::{Highlighter, wrap_line},
    utils::{new_assistant_message, new_system_message, new_user_message},
};

pub struct Conversation {
    pub messages: Vec<ChatCompletionRequestMessage>,
}

impl Conversation {
    pub fn new(system_prompt: String) -> Self {
        let system_message = new_system_message(system_prompt);
        Conversation {
            messages: vec![system_message],
        }
    }

    pub fn add_user_message(&mut self, content: String) {
        let message = new_user_message(content);
        self.messages.push(message);
    }

    pub fn add_assistant_message(&mut self, content: String) {
        let message = new_assistant_message(content);
        self.messages.push(message);
    }

    pub fn reset(&mut self) {
        self.messages = self.messages[..1].to_vec();
    }

    pub fn print_messages(&self, highlighter: &mut Highlighter) {
        self.messages.iter().for_each(|msg| match msg {
            ChatCompletionRequestMessage::User(msg) => match msg.content {
                ChatCompletionRequestUserMessageContent::Text(ref content) => {
                    println!("\n{}{}", "> ".magenta(), content.green());
                }
                _ => {}
            },
            ChatCompletionRequestMessage::Assistant(msg) => match msg.content {
                Some(ChatCompletionRequestAssistantMessageContent::Text(ref content)) => {
                    // let mut highlighter = Highlighter::new();
                    println!();
                    for line in content.split_inclusive("\n") {
                        let highlighted_line = highlighter.highlight_line(line);
                        let wrapped_line = wrap_line(&highlighted_line);
                        print!("{}", wrapped_line);
                    }
                    println!();
                }
                _ => {}
            },
            _ => {}
        })
    }

    pub fn transcript(&self) -> String {
        self.messages[1..]
            .iter()
            .map(|msg| match msg {
                ChatCompletionRequestMessage::User(user_msg) => {
                    let content = match &user_msg.content {
                        ChatCompletionRequestUserMessageContent::Text(content) => content,
                        _ => "",
                    };
                    format!("User: {}\n", content)
                }
                ChatCompletionRequestMessage::Assistant(assistant_msg) => {
                    let content = match &assistant_msg.content {
                        Some(ChatCompletionRequestAssistantMessageContent::Text(content)) => {
                            content
                        }
                        _ => "",
                    };
                    format!("Assistant: {}\n", content)
                }
                _ => "".to_string(),
            })
            .collect::<String>()
    }

    pub fn save_to_json_file(&self, path: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.messages)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn from_json_file(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let messages: Vec<ChatCompletionRequestMessage> = serde_json::from_str(&data)?;
        Ok(Conversation::from_messages(messages))
    }

    pub fn from_messages(messages: Vec<ChatCompletionRequestMessage>) -> Self {
        Conversation { messages }
    }
}
