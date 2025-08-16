/* -------------------------------------------------------------------------- */
/*                                commands.rs                                 */
/* -------------------------------------------------------------------------- */

use enum_iterator::Sequence;

#[derive(Debug, Sequence)]
pub enum Command {
    Exit,
    Clear,
    Help,
    Invalid,
}

impl Command {
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Command::Exit => vec!["exit", "quit", "q", "x"],
            Command::Clear => vec!["clear", "c"],
            Command::Help => vec!["help", "h"],
            Command::Invalid => vec![],
        }
    }
}

pub fn parse_command(input: &str) -> Option<Command> {
    let input = input.trim();
    if input.starts_with('/') {
        let parts: Vec<&str> = input[1..].split_whitespace().collect();
        if let Some(command) = parts.get(0) {
            match command.to_lowercase().as_str() {
                c if Command::Exit.strings().contains(&c) => Some(Command::Exit),
                c if Command::Clear.strings().contains(&c) => Some(Command::Clear),
                c if Command::Help.strings().contains(&c) => Some(Command::Help),
                _ => Some(Command::Invalid),
            }
        } else {
            Some(Command::Help)
        }
    } else {
        None
    }
}
