/* -------------------------------------------------------------------------- */
/*                                repl/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod commands;
mod editor;
mod mentions;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Result;
use yansi::Paint;

use crate::backend::Backend;
use crate::config::Thinking;
use crate::permissions::Permission;
use crate::protocol::{ModelInfo, RunOptions, Snapshot};
use crate::render::{self, Highlighter, Reply, Style, snailprint};
use crate::sessions::SessionId;
use crate::turn::{self, Turn};
use crate::ui;
use commands::{COMMANDS, Command, Input};
use editor::{Choices, Editor, Read, SharedStatus, Status};
use mentions::{Index, STALE_AFTER};

pub struct Repl {
    backend: Backend,
    thread: SessionId,
    options: RunOptions,
    window: Option<u32>,
    style: Style,
    greetings: bool,
    editor: Editor,
    status: SharedStatus,
    /// What the host offers to mention, and when it was fetched.
    index: Index,
    indexed: Option<Instant>,
}

pub struct Setup {
    pub backend: Backend,
    pub thread: SessionId,
    pub model: ModelInfo,
    pub thinking: Option<Thinking>,
    pub system_prompt: Option<String>,
    pub style: Style,
    pub greetings: bool,
    pub editor: crate::config::Editor,
    pub colors: bool,
}

impl Repl {
    pub async fn new(setup: Setup) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let models = setup.backend.models().await.unwrap_or_else(|error| {
            render::notice(&format!("no model completions: {error:#}"));
            Vec::new()
        });
        let choices = Choices {
            models,
            thinking: thinking_choices(),
            themes: render::themes(),
        };
        let index = Index::default();
        let editor = Editor::new(&setup.editor, setup.colors, status.clone(), choices, &index);
        let repl = Self {
            backend: setup.backend,
            thread: setup.thread,
            options: RunOptions {
                model: setup.model.reference,
                thinking: setup.thinking,
                system_prompt: setup.system_prompt,
            },
            window: setup.model.context_window,
            style: setup.style,
            greetings: setup.greetings,
            editor,
            status,
            index,
            indexed: None,
        };
        repl.show_status(None);
        repl
    }

    /// Reads and answers until the user exits, answering `first` before
    /// the first read.
    pub async fn run(mut self, first: Option<String>) -> Result<()> {
        if self.greetings {
            snailprint("\nHey!\n", 10000);
        }
        self.show_thread(false).await;
        if let Some(prompt) = first {
            render::user_message(&prompt);
            self.answer(prompt).await;
        } else {
            println!();
        }
        loop {
            self.refresh_index().await;
            let line = match self.editor.read() {
                Read::Line(line) => line,
                Read::Cancelled => continue,
                Read::Exit => break,
            };
            match commands::parse(&line) {
                Input::Empty => {}
                Input::Message(message) => self.answer(message).await,
                Input::Invalid(why) => println!("{}\n", why.yellow()),
                Input::Command(Command::Exit) => break,
                Input::Command(command) => {
                    if let Err(error) = self.execute(command).await {
                        render::error(&format!("{error:#}"));
                        println!();
                    }
                }
            }
        }
        self.backend.settle().await;
        if self.greetings {
            snailprint("\nBye!\n\n", 10000);
        }
        Ok(())
    }

    async fn answer(&mut self, prompt: String) {
        let request = turn::request(prompt, self.options.clone());
        let turn = turn::run(&self.backend, &self.thread, request, &self.style).await;
        self.update_context(&turn);
    }

    /// Fetches the host's conversations and shared files for completion,
    /// unless what was fetched is still fresh.
    async fn refresh_index(&mut self) {
        if self.indexed.is_some_and(|at| at.elapsed() < STALE_AFTER) {
            return;
        }
        let (threads, shared) = tokio::join!(self.backend.threads(), self.backend.shared());
        let mut index = mentions::lock(&self.index);
        match threads {
            Ok(threads) => index.threads = threads,
            Err(error) => render::notice(&format!("no conversation completions: {error:#}")),
        }
        match shared {
            Ok(shared) => index.shared = shared,
            Err(error) => render::notice(&format!("no shared file completions: {error:#}")),
        }
        self.indexed = Some(Instant::now());
    }

    async fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Help => help(),
            Command::New => {
                self.thread = SessionId::new();
                self.show_status(None);
                println!("{}\n", "Started a new conversation.".bold());
            }
            Command::Resume => self.resume().await?,
            Command::History => self.show_thread(true).await,
            Command::Model(reference) => self.switch_model(reference).await?,
            Command::Think(level) => self.set_thinking(level)?,
            Command::Theme(theme) => self.switch_theme(theme).await?,
            Command::Tools => self.list_tools().await?,
            Command::Memories => self.list_memories().await?,
            Command::Forget => self.forget().await?,
            Command::Clear => self.editor.clear_screen(),
            Command::Exit => {}
        }
        Ok(())
    }

    async fn resume(&mut self) -> Result<()> {
        let summaries = self.backend.threads().await?;
        if summaries.is_empty() {
            println!("{}\n", "No saved conversations yet.".bold());
            return Ok(());
        }
        let items: Vec<String> = summaries.iter().map(ToString::to_string).collect();
        if let Some(index) = ui::pick("Resume a conversation", &items, 0)?
            && let Some(summary) = summaries.into_iter().nth(index)
        {
            self.thread = summary.id;
            self.show_status(None);
            self.show_thread(true).await;
        }
        Ok(())
    }

    /// Prints the conversation so far, then follows its run if one is in
    /// progress, perhaps started on another machine. `announce_empty` says
    /// so when there is nothing.
    async fn show_thread(&mut self, announce_empty: bool) {
        let opened = match self.backend.open(&self.thread).await {
            Ok(opened) => opened,
            Err(error) => return render::error(&format!("{error:#}")),
        };
        let Snapshot { messages, running } = opened.snapshot;
        if !messages.is_empty() {
            render::conversation(&messages, &self.style).await;
        } else if running.is_none() && announce_empty {
            println!("\n{}\n", "Nothing said yet.".bold());
        }
        if let (Some(running), Some(events)) = (running, opened.events) {
            render::user_message(&running.prompt);
            let turn = turn::follow(&self.backend, &self.thread, events, &self.style).await;
            self.update_context(&turn);
        }
    }

    async fn switch_model(&mut self, reference: Option<String>) -> Result<()> {
        let reference = match reference {
            Some(reference) => reference,
            None => {
                let models = self.backend.models().await?;
                if models.is_empty() {
                    println!("{}\n", "No vendor has an API key set.".bold());
                    return Ok(());
                }
                let current = &self.options.model;
                let index = models.iter().position(|model| model == current).unwrap_or(0);
                match ui::pick("Select a model", &models, index)? {
                    Some(index) => models.into_iter().nth(index).unwrap_or_default(),
                    None => return Ok(()),
                }
            }
        };
        let model = self.backend.model(&reference).await?;
        self.options.model = model.reference;
        self.window = model.context_window;
        self.show_status(None);
        println!("{} {}\n", "Using".bold(), reference.cyan());
        Ok(())
    }

    fn set_thinking(&mut self, level: Option<String>) -> Result<()> {
        let level = match level {
            Some(level) => level,
            None => {
                let choices = thinking_choices();
                let current = self.options.thinking.map_or(DEFAULT_THINKING, Thinking::as_str);
                let index = choices.iter().position(|choice| choice == current).unwrap_or(0);
                match ui::pick("Thinking", &choices, index)? {
                    Some(index) => choices.into_iter().nth(index).unwrap_or_default(),
                    None => return Ok(()),
                }
            }
        };
        let thinking = match level.as_str() {
            DEFAULT_THINKING => None,
            level => Some(level.parse::<Thinking>().map_err(anyhow::Error::msg)?),
        };
        self.options.thinking = thinking;
        self.status.lock().unwrap_or_else(PoisonError::into_inner).thinking = thinking;
        let described = thinking.map_or("the model's default", Thinking::as_str);
        println!("{} {}\n", "Thinking:".bold(), described.cyan());
        Ok(())
    }

    async fn switch_theme(&mut self, theme: Option<String>) -> Result<()> {
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
        sample.text(SAMPLE).await;
        sample.finish().await;
        println!();
        Ok(())
    }

    async fn list_tools(&self) -> Result<()> {
        let tools = self.backend.tools(&self.thread).await?;
        println!();
        for (tool, permission) in tools.permissions {
            let permission = match permission {
                Permission::Allow => "allow".green(),
                Permission::Confirm => "confirm".yellow(),
                Permission::Deny => "deny".red(),
            };
            println!("  {permission:<8} {tool}");
        }
        if !tools.servers.is_empty() {
            println!("\n{}", "MCP servers".bold());
            for server in &tools.servers {
                match &server.tools {
                    Ok(count) => println!("  {} {} ({count} tools)", "●".green(), server.name),
                    Err(error) => println!("  {} {}: {error}", "●".red(), server.name),
                }
            }
        }
        println!();
        Ok(())
    }

    async fn list_memories(&self) -> Result<()> {
        let memories = self.backend.memories().await?;
        if memories.is_empty() {
            println!("{}\n", "Nothing remembered yet.".bold());
            return Ok(());
        }
        println!();
        for memory in memories {
            println!("  {} {memory}", "•".cyan());
        }
        println!();
        Ok(())
    }

    async fn forget(&self) -> Result<()> {
        let memories = self.backend.memories().await?;
        if memories.is_empty() {
            println!("{}\n", "Nothing remembered yet.".bold());
            return Ok(());
        }
        if let Some(index) = ui::pick("Forget a memory", &memories, 0)?
            && let Some(memory) = memories.get(index)
        {
            match self.backend.forget(memory).await? {
                true => println!("{} {}\n", "Forgot".bold(), memory.dim()),
                false => println!("{}\n", "Already forgotten.".bold()),
            }
        }
        Ok(())
    }

    fn update_context(&self, turn: &Turn) {
        if turn.context.is_some() {
            self.show_status(turn.context);
        }
    }

    fn show_status(&self, context: Option<u64>) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        *status = Status {
            remote: self.backend.remote().map(str::to_owned),
            model: self.options.model.clone(),
            thinking: self.options.thinking,
            context,
            window: self.window,
        };
    }
}

/// Leaves reasoning to the model, as an unset `thinking` does.
const DEFAULT_THINKING: &str = "default";

fn thinking_choices() -> Vec<String> {
    std::iter::once(DEFAULT_THINKING)
        .chain(Thinking::ALL.map(Thinking::as_str))
        .map(str::to_owned)
        .collect()
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
        ("Tab", "complete commands, models, themes and @ mentions"),
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
