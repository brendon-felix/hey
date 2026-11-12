/* -------------------------------------------------------------------------- */
/*                                repl/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod commands;
mod editor;

use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use yansi::Paint;

use crate::agent::{self, Assistant, Model};
use crate::config::Providers;
use crate::permissions::Permission;
use crate::render::{self, Highlighter, Reply, Style, snailprint};
use crate::sessions::{SessionId, Sessions};
use crate::{turn, ui};
use commands::{COMMANDS, Command, Input};
use editor::{Choices, Editor, Read, SharedStatus, Status};

/// How an MCP server's connection went, for `/tools`.
pub struct ServerReport {
    pub name: String,
    pub tools: Result<usize, String>,
}

pub struct Repl {
    assistant: Assistant,
    sessions: Sessions,
    session: SessionId,
    providers: Providers,
    servers: Vec<ServerReport>,
    style: Style,
    greetings: bool,
    editor: Editor,
    status: SharedStatus,
}

pub struct Setup {
    pub assistant: Assistant,
    pub sessions: Sessions,
    pub session: SessionId,
    pub providers: Providers,
    pub servers: Vec<ServerReport>,
    pub style: Style,
    pub greetings: bool,
    pub editor: crate::config::Editor,
    pub colors: bool,
}

impl Repl {
    pub fn new(setup: Setup) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let choices = Choices {
            models: agent::available_models(),
            themes: render::themes(),
        };
        let editor = Editor::new(&setup.editor, setup.colors, status.clone(), choices);
        let repl = Self {
            assistant: setup.assistant,
            sessions: setup.sessions,
            session: setup.session,
            providers: setup.providers,
            servers: setup.servers,
            style: setup.style,
            greetings: setup.greetings,
            editor,
            status,
        };
        repl.show_model(None);
        repl
    }

    /// Reads and answers until the user exits, answering `first` before
    /// the first read.
    pub async fn run(mut self, first: Option<String>) -> Result<()> {
        if self.greetings {
            snailprint("\nHey!\n", 10000);
        }
        self.print_history(false);
        if let Some(prompt) = first {
            render::user_message(&prompt);
            self.answer(&prompt).await;
        } else {
            println!();
        }
        loop {
            let line = match self.editor.read() {
                Read::Line(line) => line,
                Read::Cancelled => continue,
                Read::Exit => break,
            };
            match commands::parse(&line) {
                Input::Empty => {}
                Input::Message(message) => self.answer(&message).await,
                Input::Invalid(why) => println!("{}\n", why.yellow()),
                Input::Command(Command::Exit) => break,
                Input::Command(command) => {
                    if let Err(error) = self.execute(command) {
                        render::error(&error);
                        println!();
                    }
                }
            }
        }
        if self.greetings {
            snailprint("\nBye!\n\n", 10000);
        }
        Ok(())
    }

    async fn answer(&mut self, prompt: &str) {
        let turn = turn::run(&self.assistant, prompt, &self.session, &self.style).await;
        if turn.context.is_some() {
            self.show_model(turn.context);
        }
    }

    fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Help => help(),
            Command::New => {
                self.session = SessionId::new();
                self.show_model(None);
                println!("{}\n", "Started a new conversation.".bold());
            }
            Command::Resume => self.resume()?,
            Command::History => self.print_history(true),
            Command::Model(reference) => self.switch_model(reference)?,
            Command::Theme(theme) => self.switch_theme(theme)?,
            Command::Tools => self.list_tools(),
            Command::Clear => self.editor.clear_screen(),
            Command::Exit => {}
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        let summaries = self.sessions.list()?;
        if summaries.is_empty() {
            println!("{}\n", "No saved conversations yet.".bold());
            return Ok(());
        }
        let items: Vec<String> = summaries.iter().map(ToString::to_string).collect();
        if let Some(index) = ui::pick("Resume a conversation", &items, 0)?
            && let Some(summary) = summaries.into_iter().nth(index)
        {
            self.session = summary.id;
            self.show_model(None);
            self.print_history(true);
        }
        Ok(())
    }

    /// Prints the conversation so far; `announce_empty` says so when there is none.
    fn print_history(&self, announce_empty: bool) {
        match self.sessions.messages(&self.session) {
            Ok(messages) if messages.is_empty() => {
                if announce_empty {
                    println!("\n{}\n", "Nothing said yet.".bold());
                }
            }
            Ok(messages) => render::conversation(&messages, &self.style),
            Err(error) => render::error(&error),
        }
    }

    fn switch_model(&mut self, reference: Option<String>) -> Result<()> {
        let reference = match reference {
            Some(reference) => reference,
            None => {
                let models = agent::available_models();
                if models.is_empty() {
                    println!("{}\n", "No vendor has an API key set.".bold());
                    return Ok(());
                }
                let current = self.assistant.model().reference();
                let index = models.iter().position(|model| model == current).unwrap_or(0);
                match ui::pick("Select a model", &models, index)? {
                    Some(index) => models.into_iter().nth(index).unwrap_or_default(),
                    None => return Ok(()),
                }
            }
        };
        let model = Model::connect(&reference, &self.providers)?;
        self.assistant.set_model(model);
        self.show_model(None);
        println!("{} {}\n", "Using".bold(), reference.cyan());
        Ok(())
    }

    fn switch_theme(&mut self, theme: Option<String>) -> Result<()> {
        let theme = match theme {
            Some(theme) => theme,
            None => {
                let themes = render::themes();
                let current = self.style.theme.as_deref().unwrap_or_default();
                let index = themes.iter().position(|theme| theme == current).unwrap_or(0);
                match ui::pick("Select a theme", &themes, index)? {
                    Some(index) => themes.into_iter().nth(index).unwrap_or_default(),
                    None => return Ok(()),
                }
            }
        };
        Highlighter::new(&theme)?;
        self.style.theme = Some(theme);
        let mut sample = Reply::instant(&self.style);
        sample.text(SAMPLE);
        sample.finish();
        println!();
        Ok(())
    }

    fn list_tools(&self) {
        println!();
        for (tool, permission) in self.assistant.permissions() {
            let permission = match permission {
                Permission::Allow => "allow".green(),
                Permission::Confirm => "confirm".yellow(),
                Permission::Deny => "deny".red(),
            };
            println!("  {permission:<8} {tool}");
        }
        if !self.servers.is_empty() {
            println!("\n{}", "MCP servers".bold());
            for server in &self.servers {
                match &server.tools {
                    Ok(count) => println!("  {} {} ({count} tools)", "●".green(), server.name),
                    Err(error) => println!("  {} {}: {error}", "●".red(), server.name),
                }
            }
        }
        println!();
    }

    fn show_model(&self, context: Option<u64>) {
        let model = self.assistant.model();
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = Status {
            model: model.reference().to_owned(),
            context,
            window: model.context_window(),
        };
    }
}

fn help() {
    println!("\n{}", "Commands".bold());
    for spec in COMMANDS {
        let aliases = spec
            .aliases
            .iter()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {:<24} {} {}", spec.usage().cyan(), spec.about, aliases.dim());
    }
    println!("\n{}", "Keys".bold());
    for (keys, about) in [
        ("Tab", "complete commands, models and themes"),
        ("Ctrl+R", "search input history"),
        ("Ctrl+O", "write the message in $EDITOR"),
        ("Alt+Enter", "insert a newline"),
        ("Ctrl+C", "clear the line, or stop a running answer"),
        ("Ctrl+D", "exit"),
    ] {
        println!("  {:<24} {about}", keys.cyan());
    }
    println!();
}

const SAMPLE: &str = r#"# Sample text with code:

- Rust **function** for computing _factorial_:

    ```rust
    fn factorial(n: u32) -> u32 {
        if n == 0 { 1 } else { n * factorial(n - 1) }
    }
    ```
- Python **class** for an _API client_:

    ```python
    class APIClient:
        def __init__(self, api_key):
            self.api_key = api_key
    ```
"#;
