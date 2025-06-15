use chatgpt::prelude::*;
use chatgpt::types::Role;

pub enum Input {
    Invalid,
    Command(Command),
    Message(String),
}

pub enum Command {
    Exit,
    Clear,
    History,
    Save(String),
    Load(String),
    PrintPrompt,
    Help,
}

// pub fn get_input() -> Input {
//     use std::io::{self, Write};

//     print!("> ");
//     io::stdout().flush().unwrap();

//     let mut input = String::new();
//     if io::stdin().read_line(&mut input).is_err() {
//         return Input::Invalid;
//     }

//     let trimmed = input.trim();
//     if trimmed.is_empty() {
//         return Input::Invalid;
//     }

//     match trimmed {
//         "exit" => Input::Command(Command::Exit),
//         "clear" => Input::Command(Command::Clear),
//         "history" => Input::Command(Command::History),
//         "help" => Input::Command(Command::Help),
//         _ => Input::Message(trimmed.to_string()),
//     }
// }

pub fn clear_console() {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("cmd").args(["/c", "cls"]).status();
    } else {
        let _ = std::process::Command::new("clear").status();
    }
}

pub fn print_msg(message: &ChatMessage) {
    let role = message.role;
    let content = &message.content;
    let role_str = match role {
        Role::System => "System: ",
        Role::Assistant => "",
        Role::User => "> ",
        Role::Function => "Function: ",
    };
    println!("{}{}\n", role_str, content);
}
