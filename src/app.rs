/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::*;

use dialoguer::{Confirm, Input as DialoguerInput, Select};
use futures_util::stream::StreamExt;

use crate::commands::Command;
use crate::editor::{Editor, Input};
use crate::render::{Highlighter, animate_line, render_line, wrap_line};
use crate::utils::{clear_console, print_separator};

use yansi::Paint;

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

    fn get_line(&mut self) -> Option<String> {
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

pub struct App {
    client: Client<OpenAIConfig>,
    system_prompt: String,
    conversation: Vec<ChatCompletionRequestMessage>,
    editor: Editor,
    history_file: Option<String>,
}

impl App {
    pub fn new(api_key: &str, system_prompt: String) -> Self {
        let client = Client::new();
        let conversation = vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system_prompt.to_string())
                .build()
                .unwrap()
                .into(),
        ];
        let editor = Editor::new();
        let history_file = None;
        App {
            client,
            system_prompt,
            conversation,
            editor,
            history_file,
        }
    }

    // pub fn print_nametag(&self, name: &str) {
    //     let nametag = create_nametag(name);
    //     println!("{}", nametag);
    // }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        animate_line("\nHey!\n\n", 10000);
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Command(command) => match command {
                    Command::Exit => {
                        animate_line(&format!("\n{}\n\n", "Exiting...".red()), 5000);
                        sleep(Duration::from_millis(250));
                        return Ok(());
                    }
                    Command::Clear => {
                        animate_line(&format!("\n{}\n\n", "Clearing...".yellow()), 5000);
                        sleep(Duration::from_millis(250));
                        clear_console();
                    }
                    Command::Reset => {
                        animate_line(
                            &format!("\n{}\n", "Resetting conversation...".yellow()),
                            5000,
                        );
                        print_separator();
                        println!();
                        sleep(Duration::from_millis(500));
                        self.reset_conversation();
                    }
                    Command::Save => {
                        self.save_conversation().await?;
                    }
                    Command::Load => {
                        self.load_conversation()?;
                        self.print_conversation();
                    }
                    Command::History => {
                        self.print_conversation();
                    }
                    Command::Help => {
                        print_help();
                    }
                    Command::Invalid => {
                        animate_line(
                            &format!(
                                "\nInvalid command. Type /{} for a list of commands.\n\n",
                                "help".cyan()
                            ),
                            2000,
                        );
                    }
                },
                Input::Message(message) => {
                    self.push_user_message(&message);
                    let response = self.get_response().await?;
                    // dbg!(&response);
                    self.push_assistant_message(&response);
                }
                Input::Invalid => {}
            }
        }
    }

    pub fn push_user_message(&mut self, message: &str) {
        self.conversation.push(
            ChatCompletionRequestUserMessageArgs::default()
                .content(message)
                .build()
                .unwrap()
                .into(),
        );
    }

    pub fn push_assistant_message(&mut self, message: &str) {
        self.conversation.push(
            ChatCompletionRequestAssistantMessageArgs::default()
                .content(message)
                .build()
                .unwrap()
                .into(),
        );
    }

    pub async fn get_response(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let mut buffer = ResponseBuffer::new();
        let request = CreateChatCompletionRequestArgs::default()
            .model("gpt-4o")
            .max_tokens(512u32)
            .messages(self.conversation.clone())
            .build()?;

        let mut stream = self.client.chat().create_stream(request).await?;
        let mut response = String::new();
        // let mut lock = stdout().lock();

        let mut highlighter = Highlighter::new();
        println!();
        while let Some(result) = stream.next().await {
            match result {
                Ok(response_chunk) => {
                    if let Some(ref delta) = response_chunk.choices[0].delta.content {
                        buffer.append(delta);
                        response.push_str(delta);
                    }
                    while let Some(line) = buffer.get_line() {
                        render_line(&line, &mut highlighter);
                    }
                }
                Err(err) => {
                    animate_line(&format!("\n{} {}\n", "Error:".red(), err), 5000);
                    return Err(Box::new(err));
                }
            }
        }
        if let Some(remaining) = buffer.get_remaining() {
            if !remaining.is_empty() {
                render_line(&remaining, &mut highlighter);
            }
        }
        println!("\n");
        Ok(response)
    }

    async fn get_title(&self) -> String {
        let history_string = self.conversation[1..]
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
            .collect::<String>();
        let new_prompt = ChatCompletionRequestSystemMessageArgs::default()
            .content("You're job is to generate a title/subject line for the following conversation. The title/subject line should be around 2-6 words and should be concise enough to be used as a filename for storing the conversation history. I will now provide a transcript of the conversation and you should reply only with the title/subject line (and nothing else). Your reply will be directly used to generate the filename.")
            .build()
            .unwrap()
            .into();
        let transcript = ChatCompletionRequestUserMessageArgs::default()
            .content(history_string)
            .build()
            .unwrap()
            .into();
        let history = vec![new_prompt, transcript];
        self.client
            .chat()
            .create(
                CreateChatCompletionRequestArgs::default()
                    .model("gpt-3.5-turbo")
                    .messages(history)
                    .build()
                    .unwrap(),
            )
            .await
            .map(|response| {
                response.choices[0]
                    .message
                    .content
                    .clone()
                    .unwrap_or_else(|| "conversation".to_string())
            })
            .unwrap_or_else(|_| "conversation".to_string())
    }

    fn reset_conversation(&mut self) {
        self.conversation = vec![
            ChatCompletionRequestSystemMessageArgs::default()
                .content(self.system_prompt.clone())
                .build()
                .unwrap()
                .into(),
        ];
    }

    async fn save_conversation(&self) -> Result<(), Box<dyn std::error::Error>> {
        let serialized = serde_json::to_string(&self.conversation)?;
        let filename = if let Some(filename) = &self.history_file {
            filename.clone()
        } else {
            let title = self.get_title().await;
            let title_confirmed = Confirm::new()
                .with_prompt(&format!(
                    "\nDo you want to save the conversation with the title: {}?",
                    title.blue()
                ))
                .default(true)
                .interact()?;
            let title: String = if title_confirmed {
                title
            } else {
                DialoguerInput::new()
                    .with_prompt("Enter a filename or title for the conversation")
                    .interact_text()?
            };
            let name = if title.is_empty() {
                format!(
                    "conversation_{}",
                    chrono::Utc::now().format("%Y%m%d_%H%M%S")
                )
            } else {
                format!(
                    "{}",
                    title
                        .trim()
                        .to_lowercase()
                        .replace(' ', "_")
                        .replace(", ", "_")
                )
            };
            if name.ends_with(".json") {
                name
            } else {
                format!("{}.json", name)
            }
        };
        let filepath = filename;
        let mut file = std::fs::File::create(&filepath)?;
        file.write_all(serialized.as_bytes())?;
        animate_line(
            &format!(
                "\n{} {}.\n\n",
                "Conversation saved successfully to".green(),
                filepath.blue()
            ),
            2000,
        );
        Ok(())
    }

    fn load_conversation(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let conversations_folder = "./";

        let filename = select_conversation(conversations_folder).unwrap();

        let filepath = filename;

        self.history_file = Some(filepath.clone());
        self.conversation = serde_json::from_str(&std::fs::read_to_string(&filepath)?)?;
        animate_line(
            &format!("\nConversation loaded successfully from {}.\n", filepath),
            2000,
        );
        Ok(())
    }

    fn print_conversation(&self) {
        if self.conversation.len() <= 1 {
            animate_line("\nNo conversation history available.\n\n", 5000);
            return;
        }
        for msg in &self.conversation {
            // dbg!(&msg);
            match msg {
                ChatCompletionRequestMessage::User(msg) => match msg.content {
                    ChatCompletionRequestUserMessageContent::Text(ref content) => {
                        println!("\n{}{}", "> ".magenta(), content.green());
                    }
                    // ChatCompletionRequestUserMessageContent::Array(ref content_parts) => {
                    //     let content = content_parts
                    //         .iter()
                    //         .map(|part| match part {
                    //             ChatCompletionRequestUserMessageContentPart::Text(text) => {
                    //                 &text.text
                    //             }
                    //             _ => "",
                    //         })
                    //         .collect::<Vec<_>>()
                    //         .join("");
                    //     println!("\n{}{}", "> ".magenta(), content.green());
                    // }
                    _ => {}
                },
                ChatCompletionRequestMessage::Assistant(msg) => match msg.content {
                    Some(ChatCompletionRequestAssistantMessageContent::Text(ref content)) => {
                        // let mut highlighter = Highlighter::new();
                        let mut highlighter = Highlighter::new();
                        println!();
                        for line in content.split_inclusive("\n") {
                            let line = highlighter.highlight_line(line);
                            let line = wrap_line(&line);
                            print!("{}", line);
                        }
                        println!();
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn print_help() {
    animate_line(&format!("\n{}\n", "Available commands:".blue()), 1000);
    // animate_line("TODO\n", 10000);
    enum_iterator::all::<Command>().for_each(|command| {
        animate_line(
            &format!(
                "{}\n",
                command
                    .strings()
                    .iter()
                    .map(|s| format!("/{}", s.cyan()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            1000,
        );
    });
}

fn select_conversation(folder: &str) -> Option<String> {
    let conversation_files: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "json"))
        .map(|entry| entry.path())
        .collect();

    if conversation_files.is_empty() {
        animate_line(
            &format!("\nNo conversation files found in {}\n", folder.blue()),
            2000,
        );
        return None;
    }

    let options: Vec<String> = conversation_files
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
                .blue()
                .to_string()
        })
        .collect();

    let selection = Select::new()
        .with_prompt("\nSelect a conversation to load")
        .items(&options)
        .default(0)
        .interact()
        .unwrap();

    Some(conversation_files[selection].to_string_lossy().to_string())
}

// fn create_nametag(name: &str) -> String {
//     let length = name.to_string().len();
//     let name = Paint::new(name).magenta().italic();
//     format!(
//         "╭{}╮\n│ {} │\n╰{}╯",
//         "─".repeat(length + 2),
//         name,
//         "─".repeat(length + 2)
//     )
// }
