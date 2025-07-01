use chatgpt::prelude::*;
use chatgpt::types::Role;
use futures_util::stream::StreamExt;
use std::io::{stdout, Write};
use std::fs;
use crate::commands::{Command, Input, print_msg, clear_console};

use bat::assets::HighlightingAssets;
use yansi::Paint;
use syntect::{
    easy::HighlightLines,
    dumps::from_uncompressed_data,
    parsing::SyntaxSet
};

const SYNTAX_SET: &[u8] = include_bytes!("../syntax_set.bin");
const MAX_LINE_LENGTH: usize = 100;

pub async fn stream_single_response(client: &ChatGPT, message: String, prompt_path: String) -> Result<()> {
    let term_width = term_size::dimensions().map(|(w, _)| w).expect("Failed to get terminal width");
    let max_width = term_width.min(MAX_LINE_LENGTH);
    let ha = HighlightingAssets::from_binary();
    let syntax_set: SyntaxSet = from_uncompressed_data(SYNTAX_SET).expect("Failed to load syntax set");
    let syntax_ref = syntax_set.find_syntax_by_name("Markdown").expect("Failed to find syntax");
    let theme = ha.get_theme("ansi");
    let mut highlighter = HighlightLines::new(&syntax_ref, &theme);
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
    let mut curr_line = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            ResponseChunk::Content { delta, response_index: _ } => {
                curr_line = curr_line + &delta;
                while curr_line.contains('\n') {
                    let before_newline = curr_line.split_inclusive('\n').next().unwrap();
                    let ranges = highlighter.highlight_line(before_newline, &syntax_set).expect("Failed to highlight line");
                    let highlighted_line = ranges.iter().map(|(style, text)| {
                        let text = text.replace('\n', "");
                        match style.foreground.r {
                            // 0 => text.rgb(RESPONSE_COLOR.0, RESPONSE_COLOR.1, RESPONSE_COLOR.2),
                            0 => text.primary(),
                            1 => text.red(),
                            2 => text.green(),        // strings and stuff
                            3 => text.yellow(),     // literals and numbers
                            4 => text.blue(),       // headers and function names
                            5 => text.magenta(),    // keywords
                            6 => text.white(),
                            7 => text.black(),
                            _ => text.primary(),
                        }.to_string()
                    }).collect::<String>();
                    // }).collect::<Vec<_>>();

                    // for range in highlighted_line {
                    //     print!("{}", range);
                    //     stdout().lock().flush().unwrap();
                    //     std::thread::sleep(std::time::Duration::from_millis(5));
                    // }

                    let wrapped = textwrap::wrap(&highlighted_line, max_width);

                    // println!("{}", textwrap::wrap(&highlighted_line, term_width));
                    for line in wrapped {
                        line.chars().for_each(|c| {
                            print!("{}", c);
                            stdout().lock().flush().unwrap();
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        });
                        println!();
                        stdout().lock().flush().unwrap();
                    }
                    curr_line = curr_line.splitn(2, '\n').nth(1).unwrap_or("").to_string();
                }
                // print!("{}\r", curr_line.rgb(PARTIAL_LINE_COLOR.0, PARTIAL_LINE_COLOR.1, PARTIAL_LINE_COLOR.2));
                stdout().lock().flush().unwrap();
            }
            _ => (),
        }
    }
    let ranges = highlighter.highlight_line(&curr_line, &syntax_set).expect("Failed to highlight line");
    let highlighted_line = ranges.iter().map(|(style, text)| {
        let text = text.replace('\n', "");
        match style.foreground.r {
            // 0 => text.rgb(RESPONSE_COLOR.0, RESPONSE_COLOR.1, RESPONSE_COLOR.2),
            0 => text.primary(),
            1 => text.red(),
            2 => text.green(),
            3 => text.yellow(),
            4 => text.blue(),
            5 => text.magenta(),
            6 => text.white(),
            7 => text.black(),
            _ => text.primary(),
        }.to_string()
    }).collect::<String>();
    let wrapped = textwrap::wrap(&highlighted_line, max_width);
    for line in wrapped {
        line.chars().for_each(|c| {
            print!("{}", c);
            stdout().lock().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_micros(500));
        });
        println!();
        stdout().lock().flush().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!();
    // println!("\n");
    Ok(())
}

pub struct GPTConversation {
    pub system_prompt: String,
    pub conversation: Conversation,
}
impl GPTConversation {
    pub fn new(client: ChatGPT, system_prompt: String) -> Self {
        let conversation = client.new_conversation_directed(system_prompt.clone());
        GPTConversation { system_prompt, conversation }
    }

    pub async fn stream_next_response(&mut self, message: String) -> Result<()> {
        let term_width = term_size::dimensions().map(|(w, _)| w).expect("Failed to get terminal width");
        let max_width = term_width.min(MAX_LINE_LENGTH);
        let ha = HighlightingAssets::from_binary();
        let syntax_set: SyntaxSet = from_uncompressed_data(SYNTAX_SET).expect("Failed to load syntax set");
        let syntax_ref = syntax_set.find_syntax_by_name("Markdown").expect("Failed to find syntax");
        let theme = ha.get_theme("ansi");
        let mut highlighter = HighlightLines::new(&syntax_ref, &theme);
        let mut stream = self.conversation.send_message_streaming(message).await?;
        let mut output: Vec<ResponseChunk> = Vec::new();
        // let mut curr_line_length = 0;
        let mut curr_line = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                ResponseChunk::Content { delta, response_index } => {
                    curr_line = curr_line + &delta;
                    while curr_line.contains('\n') {
                        let before_newline = curr_line.split_inclusive('\n').next().unwrap();
                        let ranges = highlighter.highlight_line(before_newline, &syntax_set).expect("Failed to highlight line");
                        let highlighted_line = ranges.iter().map(|(style, text)| {
                            let text = text.replace('\n', "");
                            match style.foreground.r {
                                // 0 => text.rgb(RESPONSE_COLOR.0, RESPONSE_COLOR.1, RESPONSE_COLOR.2),
                                0 => text.primary(),
                                1 => text.red(),
                                2 => text.green(),        // strings and stuff
                                3 => text.yellow(),     // literals and numbers
                                4 => text.blue(),       // headers and function names
                                5 => text.magenta(),    // keywords
                                6 => text.white(),
                                7 => text.black(),
                                _ => text.primary(),
                            }.to_string()
                        }).collect::<String>();
                        // }).collect::<Vec<_>>();

                        // for range in highlighted_line {
                        //     print!("{}", range);
                        //     stdout().lock().flush().unwrap();
                        //     std::thread::sleep(std::time::Duration::from_millis(5));
                        // }

                        let wrapped = textwrap::wrap(&highlighted_line, max_width);

                        // println!("{}", textwrap::wrap(&highlighted_line, term_width));
                        for line in wrapped {
                            line.chars().for_each(|c| {
                                print!("{}", c);
                                stdout().lock().flush().unwrap();
                                std::thread::sleep(std::time::Duration::from_micros(500));
                            });
                            println!();
                            stdout().lock().flush().unwrap();
                        }
                        curr_line = curr_line.splitn(2, '\n').nth(1).unwrap_or("").to_string();
                    }
                    // print!("{}\r", curr_line.rgb(PARTIAL_LINE_COLOR.0, PARTIAL_LINE_COLOR.1, PARTIAL_LINE_COLOR.2));
                    stdout().lock().flush().unwrap();
                    output.push(ResponseChunk::Content {
                        delta,
                        response_index,
                    });
                }
                other => output.push(other),
            }
        }
        let ranges = highlighter.highlight_line(&curr_line, &syntax_set).expect("Failed to highlight line");
        let highlighted_line = ranges.iter().map(|(style, text)| {
            let text = text.replace('\n', "");
            match style.foreground.r {
                // 0 => text.rgb(RESPONSE_COLOR.0, RESPONSE_COLOR.1, RESPONSE_COLOR.2),
                0 => text.primary(),
                1 => text.red(),
                2 => text.green(),
                3 => text.yellow(),
                4 => text.blue(),
                5 => text.magenta(),
                6 => text.white(),
                7 => text.black(),
                _ => text.primary(),
            }.to_string()
        }).collect::<String>();
        let wrapped = textwrap::wrap(&highlighted_line, max_width);
        for line in wrapped {
            line.chars().for_each(|c| {
                print!("{}", c);
                stdout().lock().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_micros(500));
            });
            println!();
            stdout().lock().flush().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!();
        self.conversation.history.push(ChatMessage::from_response_chunks(output)[0].to_owned());
        Ok(())
    }

    pub fn print_history(&self) {
        for msg in &self.conversation.history[1..] {
            print_msg(msg);
        }
    }

    pub async fn save(&self, filename: &str) -> Result<()> {
        self.conversation.save_history_json(filename).await?;
        println!("Conversation saved to {}", filename);
        Ok(())
    }

    pub async fn load(&mut self, client: &ChatGPT, filename: &str) -> Result<()> {
        let loaded_conversation = client.restore_conversation_json(filename).await?;
        self.conversation = loaded_conversation;
        println!("Conversation loaded from {}", filename);
        Ok(())
    }

    pub fn clear_history(&mut self) {
        self.conversation.history = vec![
            ChatMessage {
                role: Role::System,
                content: self.system_prompt.clone(),
            }
        ];
    }
}

pub async fn conversation(client: &ChatGPT, prompt_path: String) -> Result<()> {
    let system_prompt = load_system_prompt(prompt_path);
    let mut conversation = GPTConversation::new(client.clone(), system_prompt.clone());

    // clear_console();
    loop {
        let input = get_input();
        println!();
        match input {
            Input::Message(message) => conversation.stream_next_response(message).await?,
            Input::Command(command) => {
                match command {
                    Command::Exit => {
                        println!("{}", "Exiting...".red());
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        // clear_console();
                        return Ok(());
                    }
                    Command::Clear => {
                        conversation.clear_history();
                        clear_console();
                        continue;
                    }
                    Command::History => {
                        println!("--- Start History ---\n");
                        conversation.print_history();
                        println!("---- End History ----\n");
                        continue;
                    }
                    Command::Save(path  ) => {
                        if let Err(e) = conversation.save(&path).await {
                            eprintln!("Error saving conversation: {}", e);
                        }
                        continue;
                    }
                    Command::Load(path ) => {
                        match conversation.load(client, &path).await {
                            Ok(_) => {
                                println!("Conversation loaded successfully.");
                            }
                            Err(e) => {
                                eprintln!("Error loading conversation: {}", e);
                            }
                        }
                        clear_console();
                        conversation.print_history();
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
    print!("{} {}", ">".cyan(), "".green().linger());
    stdout().lock().flush().unwrap();
    std::io::stdin().read_line(&mut input).expect("Failed to read line");
    let input = input.trim();
    print!("{}", "".resetting());
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

// pub fn highlight_code_block(code: &str, language: Option<&str>) -> String {
//     match Highlighter::new() {
//         Ok(highlighter) => {
//             match highlighter.highlight_text(code, language) {
//                 Ok(highlighted) => highlighted,
//                 Err(_) => code.to_string(), // Fallback to plain text if highlighting fails
//             }
//         }
//         Err(_) => code.to_string(), // Fallback to plain text if highlighter creation fails
//     }
// }
