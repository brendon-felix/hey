/* -------------------------------------------------------------------------- */
/*                              repl/commands.rs                              */
/* -------------------------------------------------------------------------- */

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Threads,
    Fork,
    History,
    Model(Option<String>),
    Think(Option<String>),
    Theme(Option<String>),
    Tools,
    Skills,
    Memories,
    Forget,
    Clear,
    Exit,
}

/// What a command's argument names, which decides how it completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    Model,
    Thinking,
    Theme,
}

impl Arg {
    pub fn placeholder(self) -> &'static str {
        match self {
            Arg::Model => "vendor/model",
            Arg::Thinking => "level",
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
        name: "threads",
        aliases: &["resume", "load", "l"],
        arg: None,
        about: "Browse, open, archive and delete conversations",
        build: |_| Command::Threads,
    },
    Spec {
        name: "fork",
        aliases: &[],
        arg: None,
        about: "Continue in a copy of this conversation",
        build: |_| Command::Fork,
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
        name: "think",
        aliases: &[],
        arg: Some(Arg::Thinking),
        about: "Set thinking: off, minimal…max, or default",
        build: Command::Think,
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
        name: "skills",
        aliases: &[],
        arg: None,
        about: "List skills, which /name invokes",
        build: |_| Command::Skills,
    },
    Spec {
        name: "memories",
        aliases: &["mem"],
        arg: None,
        about: "List what hey remembers across conversations",
        build: |_| Command::Memories,
    },
    Spec {
        name: "forget",
        aliases: &[],
        arg: None,
        about: "Pick a memory to forget",
        build: |_| Command::Forget,
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

/// `line` as typed: a command, or a message, which may start with `/name`
/// to invoke a skill that `is_skill` knows of. Commands win over skills of
/// the same name, which `@skill:name` still invokes.
pub fn parse(line: &str, is_skill: impl Fn(&str) -> bool) -> Input {
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
        (None, _) if is_skill(word) => Input::Message(line.to_owned()),
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
    fn parses_messages_commands_skills_and_mistakes() {
        let parse = |line| parse(line, |word| ["deploy", "new"].contains(&word));
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
        assert_eq!(parse(" /deploy to staging"), Input::Message("/deploy to staging".into()));
        assert_eq!(parse("/new"), Input::Command(Command::New), "commands win");
    }
}
