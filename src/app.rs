/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

// TODO: Add confirmation for filename and offer to change it
//

use std::thread::sleep;
use std::time::Duration;

use chatgpt::prelude::*;
use chatgpt::types::Role;
use dialoguer::Select;
use futures_util::stream::StreamExt;

use crate::commands::Command;
use crate::editor::{Editor, Input};
use crate::render::{Highlighter, animate_line, render_line};
use crate::utils::clear_console;

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
    client: ChatGPT,
    system_prompt: String,
    conversation: Conversation,
    editor: Editor,
}

impl App {
    pub fn new(api_key: &str, system_prompt: String) -> Self {
        let config = ModelConfiguration {
            engine: ChatGPTEngine::Gpt35Turbo,
            ..Default::default()
        };
        let client =
            ChatGPT::new_with_config(api_key, config).expect("Failed to create ChatGPT client");
        let conversation = client.new_conversation_directed(system_prompt.clone());
        let editor = Editor::new();
        App {
            client,
            system_prompt,
            conversation,
            editor,
        }
    }

    // pub fn print_nametag(&self, name: &str) {
    //     let nametag = create_nametag(name);
    //     println!("{}", nametag);
    // }

    pub async fn run(&mut self) -> Result<()> {
        animate_line("\nHey!\n\n", 5000);
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Command(command) => match command {
                    Command::Exit => {
                        animate_line(&format!("\n{}\n", "Exiting...".red()), 2000);
                        sleep(Duration::from_millis(100));
                        return Ok(());
                    }
                    Command::Clear => {
                        animate_line(&format!("\n{}\n", "Clearing...".yellow()), 2000);
                        sleep(Duration::from_millis(250));
                        clear_console();
                    }
                    Command::Reset => {
                        animate_line(
                            &format!("\n{}\n", "Resetting conversation...".yellow()),
                            2000,
                        );
                        sleep(Duration::from_millis(500));
                        self.reset_conversation();
                    }
                    Command::Save => match self.save_conversation().await {
                        Ok(_) => {
                            println!("\n{}", format!("Conversation saved successfully.").green());
                        }
                        Err(e) => {
                            eprintln!("\n{}", format!("Failed to save conversation: {}", e).red());
                        }
                    },
                    Command::Load => {
                        self.load_conversation().await;
                        print_history(&self.conversation.history);
                        println!();
                    }
                    Command::History => {
                        if self.conversation.history.is_empty() {
                            println!("\nNo conversation history available.\n");
                        } else {
                            println!();
                            print_history(&self.conversation.history);
                            println!();
                        }
                    }
                    Command::Help => {
                        print_help();
                    }
                    Command::Invalid => {
                        println!(
                            "\nInvalid command. Type /{} for a list of commands.",
                            "help".cyan()
                        );
                    }
                },
                Input::Message(message) => {
                    let mut response = self.get_response(message).await?;
                    self.conversation.history.append(&mut response);
                }
                Input::Invalid => {
                    println!("Invalid input. Please try again.");
                }
            }
        }
    }

    pub async fn get_response(&mut self, message: String) -> Result<Vec<ChatMessage>> {
        let mut buffer = ResponseBuffer::new();
        let mut stream = self.conversation.send_message_streaming(message).await?;
        let mut highlighter = Highlighter::new();
        let mut output: Vec<ResponseChunk> = Vec::new();
        println!();
        while let Some(chunk) = stream.next().await {
            match chunk {
                ResponseChunk::Content {
                    delta,
                    response_index,
                } => {
                    buffer.append(&delta);
                    output.push(ResponseChunk::Content {
                        delta,
                        response_index,
                    });
                    while let Some(line) = buffer.get_line() {
                        render_line(&line, &mut highlighter);
                    }
                }
                chunk => output.push(chunk),
            }
        }
        if let Some(remaining) = buffer.get_remaining() {
            if !remaining.is_empty() {
                render_line(&remaining, &mut highlighter);
            }
        }
        println!("\n");
        let response = ChatMessage::from_response_chunks(output);
        Ok(response)
    }

    async fn get_title(&self) -> String {
        let history_string = self.conversation.history[1..]
            .to_vec()
            .iter()
            .map(|msg| format!("{:?}: {}\n", msg.role, msg.content))
            .collect::<String>();
        let new_prompt = ChatMessage {
            role: Role::System,
            content: "You're job is to generate a title/subject line for the following conversation. The title/subject line should be around 2-6 words and should be concise enough to be used as a filename for storing the conversation history. I will now provide a transcript of the conversation and you should reply only with the title/subject line (and nothing else). Your reply will be directly used to generate the filename".to_string(),
        };
        let transcript = ChatMessage {
            role: Role::User,
            content: history_string,
        };
        let history = vec![new_prompt, transcript];
        let response = self
            .client
            .send_history(&history)
            .await
            .expect("Failed to get response");
        response.message().content.clone()
    }

    fn reset_conversation(&mut self) {
        let prompt = self.system_prompt.clone();
        self.conversation = self.client.new_conversation_directed(prompt);
    }

    async fn save_conversation(&self) -> Result<()> {
        let title = self.get_title().await;
        let filename = if title.is_empty() {
            "conversation.json".to_string()
        } else {
            format!("{}.json", title.to_lowercase().replace(' ', "_"))
        };
        let filepath = filename;
        self.conversation.save_history_json(&filepath).await
    }

    async fn load_conversation(&mut self) -> Option<Result<()>> {
        let conversations_folder = "./";
        let filepath = match select_conversation(conversations_folder) {
            Some(file) => file,
            None => {
                println!("\nNo conversation selected.\n");
                return None;
            }
        };
        match self.client.restore_conversation_json(&filepath).await {
            Ok(conversation) => {
                self.conversation = conversation;
                // println!("\nConversation loaded successfully from {}.\n", filepath);
                Some(Ok(()))
            }
            Err(e) => {
                eprintln!("\nFailed to load conversation from {}: {}\n", filepath, e);
                Some(Err(e))
            }
        }
    }
}

fn print_history(history: &[ChatMessage]) {
    for msg in history {
        match msg.role {
            Role::User => {
                println!("\n{}{}", "> ".magenta(), msg.content.green());
            }
            Role::Assistant => {
                let mut highlighter = Highlighter::new();
                println!();
                for line in msg.content.split_inclusive("\n") {
                    let line = highlighter.highlight_line(line);
                    print!("{}", line);
                }
                println!();
            }
            _ => {}
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
        println!("No conversation files found in {}", folder);
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
