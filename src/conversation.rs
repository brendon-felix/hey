use chatgpt::prelude::*;
use chatgpt::types::Role;
use futures_util::stream::StreamExt;
use std::io::{stdout, Write};
use std::fs;
use crate::commands::{Command, Input, print_msg, clear_console};

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
    while let Some(chunk) = stream.next().await {
        match chunk {
            ResponseChunk::Content { delta, response_index: _ } => {
                print!("{}", delta);
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
    // let mut conversation: Conversation = client.new_conversation();
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
                        println!("Exiting...");
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
                    Command::Save => {
                        let filename = "conversation.json";
                        if let Err(e) = save_conversation(&conversation, filename).await {
                            eprintln!("Error saving conversation: {}", e);
                        }
                        continue;
                    }
                    Command::Load => {
                        let filename = "conversation.json";
                        match load_conversation(&client, filename).await {
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
        "save" | "/s" => Input::Command(Command::Save),
        "load" | "/l" => Input::Command(Command::Load),
        "prompt" | "/p"=> Input::Command(Command::PrintPrompt),
        "help" | "?" | "/" => Input::Command(Command::Help),
        _ if input.is_empty() => Input::Invalid,
        _ => Input::Message(input.to_string()),
    }
}

async fn stream_next_response(conversation: &mut Conversation, message: String) -> Result<Vec<ResponseChunk>> {
    let mut stream = conversation.send_message_streaming(message).await?;
    let mut output: Vec<ResponseChunk> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            ResponseChunk::Content {
                delta,
                response_index,
            } => {
                print!("{}", delta);
                stdout().lock().flush().unwrap();
                output.push(ResponseChunk::Content {
                    delta,
                    response_index,
                });
            }
            other => output.push(other),
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("\n");
    Ok(output)
}

fn append_response(conversation: &mut Conversation, output: Vec<ResponseChunk>) {
    let messages = ChatMessage::from_response_chunks(output);
    conversation.history.push(messages[0].to_owned());
}

/* -------------------------------------------------------------------------- */

// pub fn run_file_dialog(filename: &str, directory: &Option<PathBuf>) -> Option<PathBuf> {
//     let dialog = FileDialog::new();

//     let dialog = if let Some(path) = directory {
//         dialog.set_directory(path)
//     } else {
//         dialog
//     };
//     dialog
//         .add_filter("log", &["txt", "log"])
//         .set_title("Save Log File")
//         .set_file_name(filename)
//         .save_file()
// }
// pub fn save_active_log(settings: &Settings, shared_state: &Arc<Mutex<State>>) {
//     let mut state = shared_state.lock().unwrap();
//     match state.active_log {
//         Some(ref mut log) => {
//             if log.unsaved_changes {
//                 match run_file_dialog(&log.filename, &settings.log_folder) {
//                     Some(log_path) => {
//                         log.save_as(&log_path);
//                         print_success(&format!("Saved log to {}", log_path.display()));
//                     },
//                     None => print_warning("Save operation was canceled!"),
//                 }
//             } else {
//                 print_warning("No unsaved changes to save!");
//             }
//         },
//         None => print_warning("No log started! Press `L` to start one"),
//     }
// }

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
