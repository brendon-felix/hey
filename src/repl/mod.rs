/* -------------------------------------------------------------------------- */
/*                                repl/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod commands;
mod editor;
mod mentions;
mod outbox;
mod theme_picker;
mod watch;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::Result;
use yansi::Paint;

use crate::backend::Backend;
use crate::browser;
use crate::clipboard::{Clipboard, Copied};
use crate::config::{Setting, Thinking};
use crate::mention;
use crate::permissions::Permission;
use crate::protocol::{ApprovalId, Ask, ModelInfo, RunId, RunOptions, Snapshot};
use crate::render::{self, Reply, Style, Theme, palette, screen, snailprint};
use crate::sessions::SessionId;
use crate::skills::Invocation;
use crate::steer::Steer;
use crate::transcript;
use crate::turn::{self, Follow, Outcome, Step, Turn};
use crate::ui;
use commands::{COMMANDS, Command, Input, Scope};
use editor::{Choices, Editor, Read, SharedStatus, Status};
use mentions::{Index, STALE_AFTER};
use outbox::Outbox;
use watch::Watch;

/// Reads and answers, following each answer while the next message is
/// typed.
pub struct Repl {
    session: Session,
    /// Lent to a blocking task while it reads, so it is not part of the
    /// session.
    editor: Editor,
}

/// The conversation this terminal shows, and what it is doing.
struct Session {
    backend: Backend,
    thread: SessionId,
    options: RunOptions,
    window: Option<u32>,
    style: Style,
    greetings: bool,
    /// Where settings changed here are saved.
    config_file: Option<PathBuf>,
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
    clipboard: Clipboard,
    /// The run this terminal follows.
    current: Option<Current>,
    /// Messages sent while it answers, until it takes them.
    outbox: Outbox,
    /// A question the run waits on, for once the editor gives up the
    /// terminal.
    asking: Option<(ApprovalId, Ask)>,
    /// Messages that were not sent, for the editor to take back.
    returned: Vec<String>,
}

struct Current {
    follow: Follow,
    then: Then,
}

/// What follows the end of the run this terminal follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// Once it has answered, the next queued message is sent.
    Answer,
    /// It was stopped, and nothing more is sent.
    Halt,
    /// It was stopped to send the next queued message at once.
    SendNow,
}

/// How a message is sent while an answer is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// Handed to the model as soon as it can take it.
    Steer,
    /// Sent once the answer ends.
    Queue,
    /// Sent at once, stopping the answer.
    Now,
}

enum Flow {
    Continue,
    Exit,
}

pub struct Setup {
    pub backend: Backend,
    pub thread: SessionId,
    pub model: ModelInfo,
    pub thinking: Option<Thinking>,
    pub system_prompt: Option<String>,
    pub style: Style,
    pub greetings: bool,
    pub config_file: Option<PathBuf>,
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
        let outbox = Outbox::default();
        let wake = Arc::new(AtomicBool::new(false));
        let watch = Watch::new(setup.thread.clone(), wake.clone());
        let editor = Editor::new(
            &setup.editor,
            setup.colors,
            status.clone(),
            outbox.clone(),
            choices,
            &index,
            watch.clone(),
            wake,
        );
        watch.start(setup.backend.clone(), editor.repaint_signal());
        let session = Session {
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
            config_file: setup.config_file,
            status,
            watch,
            shown: None,
            displayed: 0,
            index,
            indexed: None,
            clipboard: Clipboard::default(),
            current: None,
            outbox,
            asking: None,
            returned: Vec::new(),
        };
        session.show_status(None);
        Self { session, editor }
    }

    /// Reads and answers until the user exits, answering `first` before
    /// the first read.
    pub async fn run(self, first: Option<String>) -> Result<()> {
        let Self { mut session, mut editor } = self;
        if session.greetings {
            snailprint("\nHey!\n", 10000);
        }
        session.show_thread(false).await;
        match first {
            Some(prompt) => {
                render::user_message(&prompt);
                session.send(prompt).await;
            }
            None => println!(),
        }
        loop {
            session.prepare(&mut editor).await;
            let read;
            (editor, read) = session.read(editor).await;
            if let Flow::Exit = session.handle(read, &mut editor).await {
                break;
            }
        }
        session.leave().await;
        session.backend.settle().await;
        if session.greetings {
            snailprint("\nBye!\n\n", 10000);
        }
        Ok(())
    }
}

impl Session {
    /// Catches up with the thread and gives the editor back what was not
    /// sent, before it reads.
    async fn prepare(&mut self, editor: &mut Editor) {
        self.refresh_index().await;
        if self.current.is_none() {
            self.catch_up().await;
        }
        if !self.returned.is_empty() {
            editor.insert(&std::mem::take(&mut self.returned).join("\n"));
        }
    }

    /// Reads a line on a blocking thread, following the run meanwhile, its
    /// output above the prompt. A question the run asks breaks the read off,
    /// as does a message given back.
    async fn read(&mut self, editor: Editor) -> (Editor, Read) {
        let wake = editor.waker();
        screen::editing(editor.repaint_signal());
        let mut reading = tokio::task::spawn_blocking(move || {
            let mut editor = editor;
            let read = editor.read();
            (editor, read)
        });
        let (editor, read) = loop {
            let following = self.current.as_mut().filter(|_| self.asking.is_none());
            let step = tokio::select! {
                read = &mut reading => {
                    break read.unwrap_or_else(|error: tokio::task::JoinError| {
                        std::panic::resume_unwind(error.into_panic())
                    });
                }
                step = next(following, &self.backend, &self.thread) => step,
            };
            self.on(step, &wake).await;
        };
        // A message is shown where the model takes it, not where it was typed.
        if let Read::Line(line) = &read
            && self.current.is_some()
            && matches!(self.parse(line), Input::Empty | Input::Message(_))
        {
            editor.unprint(line);
        }
        // A read that broke off left its prompt, which the next draws again
        // in place: until then, output waits for it, as anything printed
        // here would be drawn over.
        match read {
            Read::Line(_) | Read::Interrupted(_) | Read::Exit => screen::direct(),
            Read::Queue(_) | Read::Now(_) | Read::Unqueue | Read::Woken => {}
        }
        (editor, read)
    }

    /// Acts on a step of the run followed while the editor reads, breaking
    /// the read off when the terminal or the input is needed.
    async fn on(&mut self, step: Step, wake: &Arc<AtomicBool>) {
        match step {
            Step::Drawn => {}
            Step::Steered(prompt) => self.outbox.steered(&prompt),
            Step::Ask { id, ask } => {
                self.asking = Some((id, ask));
                screen::wake_when_printed(wake.clone());
            }
            Step::Ended(outcome) => self.ended(outcome).await,
        }
        if !self.returned.is_empty() {
            wake.store(true, Ordering::Relaxed);
        }
    }

    async fn handle(&mut self, read: Read, editor: &mut Editor) -> Flow {
        match read {
            Read::Line(line) => return self.input(&line, Way::Steer, editor).await,
            Read::Queue(typed) => return self.input(&typed, Way::Queue, editor).await,
            Read::Now(typed) => return self.input(&typed, Way::Now, editor).await,
            Read::Unqueue => {
                if let Some(message) = self.outbox.unqueue() {
                    editor.insert(&message);
                }
            }
            Read::Interrupted(typed) => match self.current.is_some() {
                true => self.interrupt(typed).await,
                false => return Flow::Exit,
            },
            Read::Woken => {
                if let (Some((id, ask)), Some(current)) = (self.asking.take(), &mut self.current) {
                    // The question takes the prompt's place, and leaves it.
                    editor.erase_prompt();
                    current.follow.approve(&self.backend, &self.thread, id, &ask).await;
                }
            }
            Read::Exit => return Flow::Exit,
        }
        Flow::Continue
    }

    fn parse(&self, line: &str) -> Input {
        commands::parse(line, |word| mentions::lock(&self.index).command(word).is_some())
    }

    /// Runs a command, or sends a message the `way` it was sent. Only Enter
    /// runs a command: the other keys leave the prompt to be drawn again in
    /// place, which a command's output would be drawn over.
    async fn input(&mut self, line: &str, way: Way, editor: &mut Editor) -> Flow {
        match self.parse(line) {
            Input::Empty if way == Way::Now => self.send_now(None).await,
            Input::Empty => {}
            Input::Message(message) => self.message(message, way).await,
            Input::Command(_) | Input::Invalid(_) if way != Way::Steer => {
                render::notice("Enter runs commands");
                self.returned.push(line.to_owned());
            }
            Input::Invalid(why) => println!("{}\n", why.yellow()),
            Input::Command(Command::Exit) => return Flow::Exit,
            Input::Command(command) => {
                if let Err(error) = self.execute(command, editor).await {
                    render::error(&format!("{error:#}"));
                    println!();
                }
            }
        }
        Flow::Continue
    }

    /// Sends `message`: at once when nothing is answering, else the `way`
    /// it was sent. Once a stopped answer ends, nothing is steered.
    async fn message(&mut self, message: String, way: Way) {
        let Some(current) = &self.current else {
            // Enter leaves the message on screen; the other keys take it off.
            if way != Way::Steer {
                render::user_message(&message);
            }
            return self.send(message).await;
        };
        match (way, current.then) {
            (Way::Steer, Then::Answer) => self.steer(message).await,
            (Way::Now, _) => self.send_now(Some(message)).await,
            (Way::Steer | Way::Queue, _) => self.outbox.queue(message),
        }
    }

    /// Starts answering `prompt`, to follow while the next is typed.
    async fn send(&mut self, prompt: String) {
        let request = turn::request(prompt, self.options.clone());
        match turn::start(&self.backend, &self.thread, request, &self.style).await {
            Ok(follow) => self.follow(follow),
            Err(error) => {
                render::error(&format!("{error:#}"));
                screen::line("");
                let unsent = self.outbox.take();
                self.give_back(unsent);
            }
        }
    }

    /// Has the model answering handed `message` as soon as it can, or, when
    /// its answer is ending, sends it next.
    async fn steer(&mut self, message: String) {
        let steer = Steer {
            attachments: turn::attachments(&message),
            prompt: message.clone(),
        };
        match self.backend.steer(&self.thread, &steer).await {
            Ok(true) => self.outbox.steering(message),
            Ok(false) => self.outbox.queue_first(message),
            Err(error) => {
                render::error(&format!("{error:#}"));
                self.outbox.queue_first(message);
            }
        }
    }

    /// Stops the answer to send `message`, or the next queued, at once.
    async fn send_now(&mut self, message: Option<String>) {
        let Some(current) = &mut self.current else {
            return;
        };
        match message {
            Some(message) => self.outbox.queue_first(message),
            None if !self.outbox.has_queued() => return,
            None => {}
        }
        current.then = Then::SendNow;
        current.follow.stop(&self.backend, &self.thread).await;
    }

    /// Ctrl+C: stops the answer, and puts back in the input what was typed
    /// and what waited to be sent.
    async fn interrupt(&mut self, typed: String) {
        let Some(current) = &mut self.current else {
            return;
        };
        current.then = Then::Halt;
        current.follow.stop(&self.backend, &self.thread).await;
        let mut unsent = self.outbox.take();
        unsent.extend(Some(typed).filter(|typed| !typed.trim().is_empty()));
        self.returned.extend(unsent);
    }

    /// The run followed ended with `outcome`: the next queued message is
    /// sent if it answered, or was stopped for that; else what waited goes
    /// back to the input.
    async fn ended(&mut self, outcome: Outcome) {
        let Some(Current { follow, then }) = self.current.take() else {
            return;
        };
        let turn = follow.end(&self.backend, &self.thread, outcome).await;
        self.record(&turn);
        let unsteered = self.outbox.unsteered();
        self.give_back(unsteered);
        let send = matches!((then, outcome), (Then::Answer, Outcome::Answered) | (Then::SendNow, _));
        match send.then(|| self.outbox.next()).flatten() {
            Some(next) => {
                render::user_message(&next);
                self.send(next).await;
            }
            None => {
                let unsent = self.outbox.take();
                self.give_back(unsent);
            }
        }
        // Only now, so the next run, this terminal's own, wakes nothing.
        if self.current.is_none() {
            self.watch.set_following(false);
        }
    }

    /// Puts `messages`, which were not sent, back in the input.
    fn give_back(&mut self, messages: Vec<String>) {
        if !messages.is_empty() {
            render::notice("not sent; back in the input");
            self.returned.extend(messages);
        }
    }

    /// Follows `follow` from here on.
    fn follow(&mut self, follow: Follow) {
        self.watch.set_following(true);
        self.current = Some(Current {
            follow,
            then: Then::Answer,
        });
    }

    /// Stops following the run, which goes on, along with what was steering
    /// it; what was queued for after it goes back to the input.
    fn let_go(&mut self) {
        if self.current.take().is_some() {
            self.watch.set_following(false);
            self.outbox.unsteered();
            let unsent = self.outbox.take();
            self.give_back(unsent);
        }
    }

    /// On exit: a remote answer goes on, with what was steering it; one in
    /// this process stops.
    async fn leave(&mut self) {
        let Some(mut current) = self.current.take() else {
            return;
        };
        match self.backend.remote() {
            Some(remote) => {
                self.outbox.unsteered();
                render::notice(&format!("the answer goes on on `{remote}`"));
            }
            None => {
                current.follow.stop(&self.backend, &self.thread).await;
                render::notice("stopped the answer, which runs in this process");
            }
        }
        let unsent = self.outbox.take().len();
        if unsent > 0 {
            render::notice(&format!("{unsent} unsent message(s) dropped"));
        }
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
    /// its run in progress, if any; else the thread is then seen.
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
            (Some(_), Some(_)) if self.current.is_some() => {}
            (Some(running), Some(events)) => {
                render::user_message(&running.prompt);
                self.follow(Follow::new(events, &self.style));
            }
            _ => turn::viewed(&self.backend, &self.thread).await,
        }
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
        self.let_go();
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

    async fn execute(&mut self, command: Command, editor: &mut Editor) -> Result<()> {
        match command {
            Command::Help => help(),
            Command::New => {
                self.set_thread(SessionId::new());
                println!("{}\n", "Started a new conversation.".bold());
            }
            Command::Threads => self.browse().await?,
            Command::Fork => self.fork().await?,
            Command::History => self.show_thread(true).await,
            Command::Copy(scope) => self.copy(scope).await?,
            Command::Model(reference) => self.switch_model(reference).await?,
            Command::Think(level) => self.set_thinking(level)?,
            Command::Theme(theme) => self.switch_theme(theme, editor)?,
            Command::Tools => self.list_tools().await?,
            Command::Skills => self.list_skills().await?,
            Command::Memories => browser::memories::browse(&self.backend).await?,
            Command::Clear => editor.clear_screen(),
            Command::Exit => {}
        }
        Ok(())
    }

    /// Lists the host's conversations to open one, and leaves this one if
    /// it was deleted there.
    async fn browse(&mut self) -> Result<()> {
        let browsed = browser::threads::browse(&self.backend, Some(&self.thread)).await?;
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

    /// Copies what was said in `scope` as the Markdown it was written in, as
    /// opposed to how it shows: unwrapped, without reasoning or tool calls.
    async fn copy(&mut self, scope: Scope) -> Result<()> {
        let messages = self.backend.open(&self.thread).await?.snapshot.messages;
        let (text, what) = match scope {
            Scope::Reply => (transcript::last_reply(&messages), "the latest reply"),
            Scope::Thread => (
                Some(transcript::thread(&messages)).filter(|text| !text.is_empty()),
                "this conversation",
            ),
        };
        let Some(text) = text else {
            println!("{}\n", "Nothing to copy yet.".bold());
            return Ok(());
        };
        let what = format!("{what} ({} lines)", text.lines().count());
        match self.clipboard.copy(&text)? {
            Copied::System => println!("{}\n", format!("Copied {what}.").bold()),
            Copied::Terminal(unavailable) => {
                println!("{}", format!("Sent {what} to the terminal's clipboard.").bold());
                let why = unavailable.map_or("over SSH".to_owned(), |error| format!("no system clipboard: {error}"));
                render::notice(&format!("{why}; by OSC 52, which some terminals ignore"));
                println!();
            }
        }
        Ok(())
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
        println!("{} {}", "Using".bold(), palette().name.paint(&reference));
        self.save(Setting::Model(&reference));
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
        println!("{} {}", "Thinking:".bold(), palette().name.paint(described));
        self.save(Setting::Thinking(thinking));
        Ok(())
    }

    fn switch_theme(&mut self, theme: Option<String>, editor: &mut Editor) -> Result<()> {
        let theme = match theme {
            Some(theme) => theme,
            None => match theme_picker::pick()? {
                Some(theme) => theme,
                None => return Ok(()),
            },
        };
        render::set_theme(Theme::load(&theme)?);
        editor.restyle();
        self.style.highlight = true;
        let mut sample = Reply::new(&self.style);
        sample.text(theme_picker::SAMPLE);
        sample.finish();
        self.save(Setting::Theme(&theme));
        Ok(())
    }

    /// Saves `setting` to the config file so later sessions start with it,
    /// unless a remote host's config decides it, and says which.
    fn save(&self, setting: Setting) {
        match (&self.config_file, self.backend.remote()) {
            (_, Some(remote)) if setting.is_hosts() => {
                render::notice(&format!("for this session; `{remote}` sets the default"))
            }
            (None, _) => render::notice("for this session; there is no config file to save it to"),
            (Some(file), _) => match setting.save(file) {
                Ok(()) => render::notice(&format!("saved to {}", file.display())),
                Err(error) => {
                    render::error(&format!("cannot save it to {}: {error:#}", file.display()))
                }
            },
        }
        println!();
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

/// The next step of the run `current` follows; with none, never.
async fn next(current: Option<&mut Current>, backend: &Backend, thread: &SessionId) -> Step {
    match current {
        Some(current) => current.follow.next(backend, thread).await,
        None => std::future::pending().await,
    }
}

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
        ("Enter", "send; while answering, steer the answer"),
        ("Ctrl+Q", "queue the message for after the answer"),
        ("Ctrl+S", "send now, stopping the answer"),
        ("Alt+Up", "edit the message queued last"),
        ("Ctrl+C", "stop a running answer, or exit"),
        ("Ctrl+D", "exit"),
    ] {
        println!("  {:<24} {about}", palette.name.paint(keys));
    }
    println!();
}
