use anyhow::Result;
use async_openai::types::{
    ChatCompletionRequestMessage, CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
};
use async_openai::{Client, config::OpenAIConfig};
use crossterm::cursor;
use yansi::Paint;

use futures_util::stream::StreamExt;

use crate::render::{Highlighter, render_line, snailprint};
use crate::utils::{new_system_message, new_user_message};

struct ResponseBuffer {
    buffer: String,
}

impl ResponseBuffer {
    fn new() -> Self {
        ResponseBuffer {
            buffer: String::new(),
        }
    }

    fn append(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
    }

    fn get_line_with_ending(&mut self) -> Option<String> {
        if let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..=pos].to_string();
            self.buffer.drain(..=pos);
            Some(line)
        } else {
            None
        }
    }

    fn get_remaining(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let remaining = self.buffer.clone();
            self.buffer = String::new();
            Some(remaining)
        } else {
            None
        }
    }
}

pub fn create_request(
    model: &str,
    max_tokens: u32,
    messages: Vec<ChatCompletionRequestMessage>,
) -> Result<CreateChatCompletionRequest> {
    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .max_tokens(max_tokens)
        .messages(messages)
        .build()?;
    Ok(request)
}

pub async fn stream_response(
    client: &Client<OpenAIConfig>,
    request: CreateChatCompletionRequest,
    highlighter: &mut Highlighter,
) -> Result<String> {
    let mut buffer = ResponseBuffer::new();

    let mut stream = client.chat().create_stream(request).await?;
    let mut full_response = String::new();

    print!("{}\n", cursor::Hide);

    while let Some(result) = stream.next().await {
        match result {
            Ok(response_chunk) => {
                if let Some(ref delta) = response_chunk.choices[0].delta.content {
                    buffer.append(delta);
                    full_response.push_str(delta);
                }
                while let Some(line) = buffer.get_line_with_ending() {
                    if let Err(e) = render_line(&line, highlighter) {
                        let _ = snailprint(
                            &format!("\n{} {}\n", "Error rendering line:".red(), e),
                            5000,
                        );
                    }
                }
            }
            Err(err) => {
                let _ = snailprint(&format!("\n{} {}\n", "Error:".red(), err), 5000);
                return Err(err.into());
            }
        }
    }
    if let Some(remaining) = buffer.get_remaining() {
        let _ = render_line(&remaining, highlighter);
    }

    print!("\n{}\n", cursor::Show);
    Ok(full_response)
}

pub async fn generate_title(client: &Client<OpenAIConfig>, transcript: String) -> Result<String> {
    let prompt = format!(
        "Generate a concise title (max 5 words) for the following conversation (to be used in a filename). Do not use any special characters.\n"
    );
    let messages = vec![new_system_message(prompt), new_user_message(transcript)];
    let request = create_request("gpt-3.5-turbo", 10u32, messages)?;

    let response = client.chat().create(request).await?;
    if let Some(title) = response.choices.get(0) {
        if let Some(content) = &title.message.content {
            return Ok(content.trim().to_string());
        }
    }
    Ok("Untitled Conversation".to_string())
}
