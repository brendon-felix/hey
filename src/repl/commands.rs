/* -------------------------------------------------------------------------- */
/*                              repl/commands.rs                              */
/* -------------------------------------------------------------------------- */

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Resume,
    History,
    Model(Option<String>),
    Theme(Option<String>),
    Tools,
    Clear,
    Exit,
}

/// What a command's argument names, which decides how it completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    Model,
    Theme,
}

impl Arg {
    pub fn placeholder(self) -> &'static str {
        match self {
            Arg::Model => "vendor/model",
            Arg::Theme => "theme",
        }
    }
}

pub struct Spec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub arg: Option<Arg>,
    pub about: &'static str,
    build: fn(Option<String>) -> Command,
}

impl Spec {
    pub fn usage(&self) -> String {
        match self.arg {
            Some(arg) => format!("/{} [{}]", self.name, arg.placeholder()),
            None => format!("/{}", self.name),
        }
    }
}

pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "help",
        aliases: &["h"],
        arg: None,
        about: "Show commands and shortcuts",
        build: |_| Command::Help,
    },
    Spec {
        name: "new",
        aliases: &["reset", "r"],
        arg: None,
        about: "Start a new conversation",
        build: |_| Command::New,
    },
    Spec {
        name: "resume",
        aliases: &["load", "l"],
        arg: None,
        about: "Continue a saved conversation",
        build: |_| Command::Resume,
    },
    Spec {
        name: "history",
        aliases: &[],
        arg: None,
        about: "Show this conversation",
        build: |_| Command::History,
    },
    Spec {
        name: "model",
        aliases: &["m"],
        arg: Some(Arg::Model),
        about: "Switch model",
        build: Command::Model,
    },
    Spec {
        name: "theme",
        aliases: &["t"],
        arg: Some(Arg::Theme),
        about: "Switch highlighting theme",
        build: Command::Theme,
    },
    Spec {
        name: "tools",
        aliases: &[],
        arg: None,
        about: "List tools, their permissions and MCP servers",
        build: |_| Command::Tools,
    },
    Spec {
        name: "clear",
        aliases: &["c"],
        arg: None,
        about: "Clear the screen",
        build: |_| Command::Clear,
    },
    Spec {
        name: "exit",
        aliases: &["quit", "q", "x"],
        arg: None,
        about: "Exit hey",
        build: |_| Command::Exit,
    },
];

pub fn find(word: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|spec| {
        std::iter::once(&spec.name)
            .chain(spec.aliases)
            .any(|name| name.eq_ignore_ascii_case(word))
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Empty,
    Message(String),
    Command(Command),
    Invalid(String),
}

pub fn parse(line: &str) -> Input {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return match line {
            "" => Input::Empty,
            message => Input::Message(message.to_owned()),
        };
    };
    let (word, arg) = match rest.split_once(char::is_whitespace) {
        Some((word, arg)) => (word, Some(arg.trim()).filter(|arg| !arg.is_empty())),
        None => (rest, None),
    };
    match (find(word), arg) {
        (None, _) if word.is_empty() => Input::Command(Command::Help),
        (None, _) => Input::Invalid(format!("unknown command /{word}")),
        (Some(spec), Some(_)) if spec.arg.is_none() => {
            Input::Invalid(format!("/{} takes no argument", spec.name))
        }
        (Some(spec), arg) => Input::Command((spec.build)(arg.map(str::to_owned))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages_commands_and_mistakes() {
        assert_eq!(parse("  "), Input::Empty);
        assert_eq!(parse(" hi there "), Input::Message("hi there".into()));
        assert_eq!(parse("/"), Input::Command(Command::Help));
        assert_eq!(parse("/Q"), Input::Command(Command::Exit));
        assert_eq!(parse("/model"), Input::Command(Command::Model(None)));
        assert_eq!(
            parse("/m  openai/gpt-5.5 "),
            Input::Command(Command::Model(Some("openai/gpt-5.5".into())))
        );
        assert_eq!(parse("/new now"), Input::Invalid("/new takes no argument".into()));
        assert_eq!(parse("/nope"), Input::Invalid("unknown command /nope".into()));
    }
}
