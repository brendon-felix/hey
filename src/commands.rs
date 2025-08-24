/* -------------------------------------------------------------------------- */
/*                                commands.rs                                 */
/* -------------------------------------------------------------------------- */

use enum_iterator::Sequence;

#[derive(Debug, Sequence, PartialEq, Clone, Copy)]
pub enum Command {
    Exit,
    Clear,
    Reset,
    SelectModel,
    SelectTheme,
    Save,
    Load,
    History,
    Help,
    Invalid,
}

impl Command {
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Command::Exit => vec!["exit", "quit", "q", "x"],
            Command::Clear => vec!["clear", "c"],
            Command::Reset => vec!["reset", "r"],
            Command::SelectModel => vec!["model", "m"],
            Command::SelectTheme => vec!["theme", "t"],
            Command::Save => vec!["save", "s"],
            Command::Load => vec!["load", "l"],
            Command::History => vec!["history"],
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
            enum_iterator::all::<Command>()
                .find(|cmd| cmd.strings().contains(&command.to_lowercase().as_str()))
                .or(Some(Command::Invalid))
        } else {
            Some(Command::Help)
        }
    } else {
        None
    }
}
