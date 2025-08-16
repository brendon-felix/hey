/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use chatgpt::prelude::*;
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
                Input::Command(command) => {
                    match command {
                        Command::Exit => {
                            animate_line(&format!("\n{}", "Exiting...".red()), 2000);
                            sleep(Duration::from_millis(100));
                            return Ok(());
                        }
                        Command::Clear => {
                            // println!("\n{}", "Clearing...".yellow());
                            animate_line(&format!("\n{}", "Clearing...".yellow()), 2000);
                            sleep(Duration::from_millis(250));
                            clear_console();
                            self.reset_conversation();
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
                    }
                }
                Input::Message(message) => {
                    self.get_response(message).await?;
                }
                Input::Invalid => {
                    println!("Invalid input. Please try again.");
                }
            }
        }
    }

    pub async fn get_response(&mut self, message: String) -> Result<()> {
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
        Ok(())
    }

    fn reset_conversation(&mut self) {
        let prompt = self.system_prompt.clone();
        self.conversation = self.client.new_conversation_directed(prompt);
    }
}

fn print_help() {
    animate_line(&format!("\n{}\n", "Available commands:".blue()), 1000);
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
