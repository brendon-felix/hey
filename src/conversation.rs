use chatgpt::prelude::*;
use chatgpt::types::Role;
use futures_util::stream::StreamExt;
use std::io::{stdout, Write};
use std::fs;
use crate::commands::{Command, Input, print_msg, clear_console};

use colored::Colorize;

// const COLORS: [[u8; 3]; 6] = [
//     [224, 108, 117], // Red
//     [152, 195, 121], // Green
//     [97, 175, 239],  // Blue
//     [86, 182, 194],  // Cyan
//     [198, 120, 221], // Magenta
//     [229, 192, 123], // Yellow
// ];

// const COLORS: [[u8; 3]; 6] = [
//     [255, 0, 0], // Red
//     [0, 255, 0], // Green
//     [0, 0, 255],  // Blue
//     [225, 225, 0],  // Cyan
//     [255, 0, 255], // Magenta
//     [0, 255, 255], // Yellow
// ];

const MAX_LINE_LENGTH: usize = 80;

pub async fn stream_single_response(client: &ChatGPT, message: String, prompt_path: String) -> Result<()> {
    let system_prompt = load_system_prompt(prompt_path);
    let history: Vec<ChatMessage> = vec![
        ChatMessage {
            role: Role::System,
            content: system_prompt,
        },
        ChatMessage {
            role: Role::User,
            content: message,
        },
    ];
    let mut stream = client.send_history_streaming(&history).await?;
    println!();
    let mut curr_line_length = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            ResponseChunk::Content { delta, response_index: _ } => {
                let printed_token = match delta.as_str() {
                    t if t.starts_with('\n') => {
                        curr_line_length = delta.len() - 1;
                        format!("{}", delta)
                    }
                    t if t.ends_with('\n') => {
                        curr_line_length = 0;
                        format!("{}", delta)
                    }
                    t if t.contains('\n') => {
                        curr_line_length = delta.len() - delta.rfind('\n').unwrap_or(0) - 1;
                        format!("{}", delta)
                    }
                    t => {
                        if (curr_line_length + delta.len()) > MAX_LINE_LENGTH {
                            match t {
                                "." | ". " | "," | ", " | "!" | "! " | "?" | "? " => {
                                    curr_line_length = delta.len();
                                    format!("{}\n", delta)
                                }
                                _ => {
                                    curr_line_length = delta.trim_start().len();
                                    format!("\n{}", delta.trim_start())
                                }
                            }
                        } else {
                            curr_line_length += delta.len();
                            format!("{}", delta)
                        }
                    }
                };
                print!("{}", printed_token.blue());
                // print!("{}", delta);
                stdout().lock().flush().unwrap();
            }
            _ => {}
        }
    }
    println!("\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    Ok(())
}

pub async fn conversation(client: &ChatGPT, prompt_path: String) -> Result<()> {
    let system_prompt = load_system_prompt(prompt_path);
    let mut conversation: Conversation = client.new_conversation_directed(system_prompt.clone());
    // clear_console();
    loop {
        let input = get_input();
        println!();
        match input {
            Input::Message(message) => {
                let output = stream_next_response(&mut conversation, message).await?;
                append_response(&mut conversation, output);
            }
            Input::Command(command) => {
                match command {
                    Command::Exit => {
                        println!("{}", "Exiting...".red());
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        // clear_console();
                        return Ok(());
                    }
                    Command::Clear => {
                        // conversation.history.clear();
                        conversation.history = vec![
                            ChatMessage {
                                role: Role::System,
                                content: system_prompt.clone(),
                            },
                        ];
                        clear_console();
                        continue;
                    }
                    Command::History => {
                        println!("--- Start History ---\n");
                        print_history(&conversation);
                        println!("---- End History ----\n");
                        continue;
                    }
                    Command::Save(path  ) => {
                        if let Err(e) = save_conversation(&conversation, &path).await {
                            eprintln!("Error saving conversation: {}", e);
                        }
                        continue;
                    }
                    Command::Load(path ) => {
                        // let filename = "conversation.json";
                        match load_conversation(&client, &path).await {
                            Ok(loaded_conversation) => {
                                conversation = loaded_conversation;
                                println!("Conversation loaded successfully.");
                            }
                            Err(e) => {
                                eprintln!("Error loading conversation: {}", e);
                            }
                        }
                        clear_console();
                        print_history(&conversation);
                        continue;
                    }
                    Command::PrintPrompt => {
                        println!("System prompt:\n{}", system_prompt);
                        continue;
                    }
                    Command::Help => {
                        println!("Type your message and press Enter to send it.");
                        println!("Type 'clear' or '/c' to clear the conversation.");
                        println!("Type 'history' or '/h' to view the conversation history.");
                        println!("Type 'prompt' or '/p' to view the system prompt.");
                        println!("Type 'exit', 'quit', '/q', '/x' to exit the program.");
                        println!("Type 'help', '?', or '/' for this help message.");
                        println!("\n");
                        continue;
                    }
                }
            }
            Input::Invalid => {
                println!("Please enter a valid message.");
                continue;
            }
        };
    }
}

fn load_system_prompt(prompt_path: String) -> String {
    fs::read_to_string(prompt_path).unwrap_or_else(|_| {
        eprintln!("Failed to read system prompt from file. Please ensure the file exists.");
        std::process::exit(1);
    })
}

fn get_input() -> Input {
    let mut input = String::new();
    print!("> ");
    stdout().lock().flush().unwrap();   
    std::io::stdin().read_line(&mut input).expect("Failed to read line");
    let input = input.trim();
    match input {
        "exit" | "quit" | "/q" | "/x" => Input::Command(Command::Exit),
        "clear" | "/c" => Input::Command(Command::Clear),
        "history" | "/h" => Input::Command(Command::History),
        i if i.starts_with("save ") || i.starts_with("/s ") => {
            let path = i.trim_start_matches("save ").trim_start_matches("/s ");
            if path.is_empty() {
                eprintln!("Please provide a filename to save the conversation.");
                return Input::Invalid;
            }
            Input::Command(Command::Save(path.to_string()))
        },
        i if i.starts_with("load ") || i.starts_with("/l ") => {
            let path = i.trim_start_matches("load ").trim_start_matches("/l ");
            if path.is_empty() {
                eprintln!("Please provide a filename to load the conversation.");
                return Input::Invalid;
            }
            Input::Command(Command::Load(path.to_string()))
        },
        "prompt" | "/p"=> Input::Command(Command::PrintPrompt),
        "help" | "?" | "/" => Input::Command(Command::Help),
        _ if input.is_empty() => Input::Invalid,
        _ => Input::Message(input.to_string()),
    }
}

async fn stream_next_response(conversation: &mut Conversation, message: String) -> Result<Vec<ResponseChunk>> {
    let mut stream = conversation.send_message_streaming(message).await?;
    let mut output: Vec<ResponseChunk> = Vec::new();
    let mut curr_line_length = 0;
    // let mut idx = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            ResponseChunk::Content {
                delta,
                response_index,
            } => {
                // let color = [224, 108, 117];
                // let color = COLORS[idx % COLORS.len()];
                let printed_token = match delta.as_str() {
                    t if t.starts_with('\n') => {
                        curr_line_length = delta.len() - 1;
                        format!("{}", delta)
                    }
                    t if t.ends_with('\n') => {
                        curr_line_length = 0;
                        format!("{}", delta)
                    }
                    t if t.contains('\n') => {
                        curr_line_length = delta.len() - delta.rfind('\n').unwrap_or(0) - 1;
                        format!("{}", delta)
                    }
                    t => {
                        if (curr_line_length + delta.len()) > MAX_LINE_LENGTH {
                            match t {
                                "." | ". " | "," | ", " | "!" | "! " | "?" | "? " => {
                                    curr_line_length = delta.len();
                                    format!("{}\n", delta)
                                }
                                _ => {
                                    curr_line_length = delta.trim_start().len();
                                    format!("\n{}", delta.trim_start())
                                }
                            }
                        } else {
                            curr_line_length += delta.len();
                            format!("{}", delta)
                        }
                    }
                };
                print!("{}", printed_token.cyan());
                // match idx % 6 {
                //     0 => print!("{}", printed_token.red()),
                //     1 => print!("{}", printed_token.green()),
                //     2 => print!("{}", printed_token.blue()),
                //     3 => print!("{}", printed_token.magenta()),
                //     4 => print!("{}", printed_token.yellow()),
                //     5 => print!("{}", printed_token.cyan()),
                //     _ => print!("{}", printed_token),
                // }
                stdout().lock().flush().unwrap();
                output.push(ResponseChunk::Content {
                    delta,
                    response_index,
                });
            }
            other => output.push(other),
        }
        // idx += 1;
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("\n");
    Ok(output)
}

fn append_response(conversation: &mut Conversation, output: Vec<ResponseChunk>) {
    let messages = ChatMessage::from_response_chunks(output);
    conversation.history.push(messages[0].to_owned());
}

pub fn print_history(conversation: &Conversation) {
    for msg in &conversation.history[1..] {
        print_msg(msg);
    }
}

pub async fn save_conversation(conversation: &Conversation, filename: &str) -> Result<()> {
    conversation.save_history_json(filename).await?;
    println!("Conversation saved to {}", filename);
    Ok(())
}

pub async fn load_conversation(client: &ChatGPT, filename: &str) -> Result<Conversation> {
    let conversation = client
        .restore_conversation_json(filename)
        .await?;
    println!("Conversation loaded from {}", filename);
    Ok(conversation)
}
