/* -------------------------------------------------------------------------- */
/*                                repl/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod commands;
mod editor;
mod mentions;
mod theme_picker;
mod watch;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Result;
use yansi::Paint;

use crate::backend::{Backend, Events};
use crate::browser;
use crate::config::Thinking;
use crate::mention;
use crate::permissions::Permission;
use crate::protocol::{ModelInfo, RunId, RunOptions, Snapshot};
use crate::render::{self, Reply, Style, Theme, palette, snailprint};
use crate::sessions::SessionId;
use crate::skills::Invocation;
use crate::turn::{self, Turn};
use crate::ui;
use commands::{COMMANDS, Command, Input};
use editor::{Choices, Editor, Read, SharedStatus, Status};
use mentions::{Index, STALE_AFTER};
use watch::Watch;

pub struct Repl {
    backend: Backend,
    thread: SessionId,
    options: RunOptions,
    window: Option<u32>,
    style: Style,
    greetings: bool,
    editor: Editor,
    status: SharedStatus,
    /// What the host's threads are doing.
    watch: Watch,
    /// The latest run of this thread this terminal knows of, so it shows
    /// each run once.
    shown: Option<RunId>,
    /// How many of this thread's saved messages are on screen.
    displayed: usize,
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
        let wake = Arc::new(AtomicBool::new(false));
        let watch = Watch::new(setup.thread.clone(), wake.clone());
        let mut editor = Editor::new(
            &setup.editor,
            setup.colors,
            status.clone(),
            choices,
            &index,
            watch.clone(),
            wake,
        );
        watch.start(setup.backend.clone(), editor.repaint_signal());
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
            watch,
            shown: None,
            displayed: 0,
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
            self.catch_up().await;
            let line = match self.editor.read() {
                Read::Line(line) => line,
                Read::Woken => continue,
                Read::Exit => break,
            };
            match commands::parse(&line, |word| mentions::lock(&self.index).command(word).is_some()) {
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
        self.record(&turn);
    }

    /// Shows a run of this thread this terminal has not, such as one
    /// started in another terminal or on another machine.
    async fn catch_up(&mut self) {
        self.watch.rest();
        let Some(run) = self.watch.activity(&self.thread).and_then(|activity| activity.run) else {
            return;
        };
        if self.shown == Some(run) {
            return;
        }
        // Once per run, even when showing it fails, rather than retried.
        self.shown = Some(run);
        self.sync(false).await;
    }

    /// Prints what this thread saved that is not on screen, then follows
    /// its run in progress, if any; either way the thread is then seen.
    /// `announce_empty` says so when there is nothing at all.
    async fn sync(&mut self, announce_empty: bool) {
        let opened = match self.backend.open(&self.thread).await {
            Ok(opened) => opened,
            Err(error) => return render::error(&format!("{error:#}")),
        };
        let Snapshot { messages, running } = opened.snapshot;
        let new = messages.get(self.displayed..).unwrap_or_default();
        if !new.is_empty() {
            render::conversation(new, &self.style);
        } else if messages.is_empty() && running.is_none() && announce_empty {
            println!("\n{}\n", "Nothing said yet.".bold());
        }
        self.displayed = messages.len();
        match (running, opened.events) {
            (Some(running), Some(events)) => self.follow(&running.prompt, events).await,
            _ => turn::viewed(&self.backend, &self.thread).await,
        }
    }

    async fn follow(&mut self, prompt: &str, events: Events) {
        render::user_message(prompt);
        let turn = turn::follow(&self.backend, &self.thread, events, &self.style).await;
        self.record(&turn);
    }

    fn record(&mut self, turn: &Turn) {
        self.shown = turn.run.or(self.shown);
        self.displayed = turn.saved.unwrap_or(self.displayed);
        if turn.context.is_some() {
            self.show_status(turn.context);
        }
    }

    /// Shows `thread` from here on; nothing of it is on screen yet.
    fn set_thread(&mut self, thread: SessionId) {
        self.watch.set_current(thread.clone());
        self.shown = self.watch.activity(&thread).and_then(|activity| activity.run);
        self.displayed = 0;
        self.thread = thread;
        self.show_status(None);
    }

    /// Fetches the host's conversations, shared files and skills for
    /// completion, unless what was fetched is still fresh. Tells of skills
    /// that newly failed to load.
    async fn refresh_index(&mut self) {
        if self.indexed.is_some_and(|at| at.elapsed() < STALE_AFTER) {
            return;
        }
        let (threads, shared, skills) = tokio::join!(
            self.backend.threads(),
            self.backend.shared(),
            self.backend.skills()
        );
        let mut index = mentions::lock(&self.index);
        match threads {
            Ok(threads) => index.threads = threads.into_iter().map(|thread| thread.summary).collect(),
            Err(error) => render::notice(&format!("no conversation completions: {error:#}")),
        }
        match shared {
            Ok(shared) => index.shared = shared,
            Err(error) => render::notice(&format!("no shared file completions: {error:#}")),
        }
        match skills {
            Ok(skills) => {
                for problem in skills.problems.iter().filter(|problem| !index.skills.problems.contains(problem)) {
                    render::notice(&format!("skill not loaded: {problem}"));
                }
                index.skills = skills;
            }
            Err(error) => render::notice(&format!("no skill completions: {error:#}")),
        }
        self.indexed = Some(Instant::now());
    }

    async fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Help => help(),
            Command::New => {
                self.set_thread(SessionId::new());
                println!("{}\n", "Started a new conversation.".bold());
            }
            Command::Threads => self.browse().await?,
            Command::Fork => self.fork().await?,
            Command::History => self.show_thread(true).await,
            Command::Model(reference) => self.switch_model(reference).await?,
            Command::Think(level) => self.set_thinking(level)?,
            Command::Theme(theme) => self.switch_theme(theme)?,
            Command::Tools => self.list_tools().await?,
            Command::Skills => self.list_skills().await?,
            Command::Memories => self.list_memories().await?,
            Command::Forget => self.forget().await?,
            Command::Clear => self.editor.clear_screen(),
            Command::Exit => {}
        }
        Ok(())
    }

    /// Lists the host's conversations to open one, and leaves this one if
    /// it was deleted there.
    async fn browse(&mut self) -> Result<()> {
        let browsed = browser::browse(&self.backend, Some(&self.thread)).await?;
        match browsed.open {
            Some(thread) => {
                self.set_thread(thread);
                self.show_thread(true).await;
            }
            None if browsed.deleted.contains(&self.thread) => {
                self.set_thread(SessionId::new());
                println!("{}\n", "Deleted this conversation; started a new one.".bold());
            }
            None => {}
        }
        self.indexed = None;
        Ok(())
    }

    /// Goes on in a copy of this conversation, leaving the original as it is.
    async fn fork(&mut self) -> Result<()> {
        let copy = self.backend.fork(&self.thread).await?;
        // The copy holds what is on screen, so nothing of it is printed again.
        let displayed = self.displayed;
        self.set_thread(copy);
        self.displayed = displayed;
        self.indexed = None;
        println!(
            "{}\n",
            "Forked this conversation; you are now in the copy.".bold()
        );
        Ok(())
    }

    /// Prints the whole conversation, then follows its run if one is in
    /// progress. `announce_empty` says so when there is nothing.
    async fn show_thread(&mut self, announce_empty: bool) {
        self.displayed = 0;
        self.sync(announce_empty).await;
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
        println!("{} {}\n", "Using".bold(), palette().name.paint(&reference));
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
        println!("{} {}\n", "Thinking:".bold(), palette().name.paint(described));
        Ok(())
    }

    fn switch_theme(&mut self, theme: Option<String>) -> Result<()> {
        let theme = match theme {
            Some(theme) => theme,
            None => match theme_picker::pick()? {
                Some(theme) => theme,
                None => return Ok(()),
            },
        };
        render::set_theme(Theme::load(&theme)?);
        self.editor.restyle();
        self.style.highlight = true;
        let mut sample = Reply::new(&self.style);
        sample.text(theme_picker::SAMPLE);
        sample.finish();
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

    /// The host's skills, how each is invoked, and why any were not loaded.
    async fn list_skills(&self) -> Result<()> {
        let found = self.backend.skills().await?;
        let palette = palette();
        if found.skills.is_empty() && found.problems.is_empty() {
            println!(
                "\n{}",
                "No skills. A skill is a folder holding a SKILL.md, in one of:".bold()
            );
            for (source, dir) in &found.roots {
                println!("  {} {}", dir.display(), palette.muted.paint(source.as_str()));
            }
            println!();
            return Ok(());
        }
        println!();
        let index = mentions::lock(&self.index);
        for skill in &found.skills {
            let invoked = match index.command(skill.name.as_str()) {
                Some(_) => format!("/{}", skill.name),
                None => format!("@{}{}", mention::SKILL, skill.name),
            };
            let who = match skill.invocation {
                Invocation::Auto => "",
                Invocation::Manual => ", only when you invoke it",
            };
            let note = format!("{}{who}", skill.source.as_str());
            let room = render::terminal_width().saturating_sub(29 + note.chars().count());
            println!(
                "  {} {} {}",
                palette.name.paint(&format!("{invoked:<24}")),
                render::clip(&skill.description, room),
                palette.muted.paint(&note)
            );
        }
        if !found.problems.is_empty() {
            println!("\n{}", "Not loaded".bold());
            for problem in &found.problems {
                println!("  {} {problem}", "✗".red());
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
            println!("  {} {memory}", palette().name.paint("•"));
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
                true => println!("{} {}\n", "Forgot".bold(), palette().muted.paint(memory)),
                false => println!("{}\n", "Already forgotten.".bold()),
            }
        }
        Ok(())
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
    let palette = palette();
    println!("\n{}", "Commands".bold());
    for spec in COMMANDS {
        let aliases = spec
            .aliases
            .iter()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {:<24} {} {}",
            palette.name.paint(&spec.usage()),
            spec.about,
            palette.muted.paint(&aliases)
        );
    }
    println!("\n{}", "Keys".bold());
    for (keys, about) in [
        ("Tab", "complete commands, models, themes and @ mentions"),
        ("Ctrl+R", "search input history"),
        ("Ctrl+O", "write the message in $EDITOR"),
        ("Alt+Enter", "insert a newline"),
        ("Ctrl+C", "stop a running answer, or exit"),
        ("Ctrl+D", "exit"),
    ] {
        println!("  {:<24} {about}", palette.name.paint(keys));
    }
    println!();
}
