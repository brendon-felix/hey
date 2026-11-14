/* -------------------------------------------------------------------------- */
/*                               repl/editor.rs                               */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossterm::cursor::{MoveToColumn, MoveUp, SetCursorStyle};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use fuzzy_matcher::skim::SkimMatcherV2;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer, CompletionResult, CursorConfig, DefaultHinter, EditCommand,
    EditMode, Emacs, FileBackedHistory, Helix, Highlighter, Hinter, History, HistoryItem, KeyCode,
    KeyModifiers, Keybindings, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, PromptHelixMode, PromptViMode, Reedline, ReedlineEvent,
    ReedlineMenu, RepaintSignal, Signal, Span, StatusLine, StyledText, Suggestion, Vi,
    default_emacs_keybindings,
    default_helix_insert_keybindings, default_helix_normal_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
};
use textwrap::core::display_width;

use super::commands::{self, Arg, COMMANDS};
use super::mentions::{self as host, HostIndex, Index, Mentions};
use super::outbox::Outbox;
use super::watch::{self, Watch};
use crate::config::{self, EditMode as Mode, RightPrompt, Thinking};
use crate::mention::{self, Mention, Reference};
use crate::render::{self, Palette, palette, screen};

const MENU: &str = "completion_menu";

/// How often the editor looks for output to show while it waits for keys,
/// and so how smoothly an answer streams above it.
const POLL: Duration = Duration::from_millis(33);

/// Columns the prompt indicator, `> ` or another, takes before the input.
const INDICATOR: usize = 2;

/// What the right prompt shows: where the agent runs and how full its context is.
#[derive(Debug, Default)]
pub struct Status {
    /// The remote host, when the agent runs on one.
    pub remote: Option<String>,
    pub model: String,
    pub thinking: Option<Thinking>,
    pub context: Option<u64>,
    pub window: Option<u32>,
}

impl Status {
    fn short(&self) -> String {
        let name = short_name(&self.model);
        let model = match self.thinking {
            Some(level) => format!("{name} {}", level.as_str()),
            None => name.to_owned(),
        };
        let context = self.context.map(|used| match self.window {
            Some(window) => format!("{}/{}", render::tokens(used), render::tokens(u64::from(window))),
            None => render::tokens(used),
        });
        self.after_remote(std::iter::once(model).chain(context))
    }

    fn full(&self) -> String {
        let thinking = self.thinking.map(|level| format!("thinking {}", level.as_str()));
        let context = self.context.map(|used| match self.window {
            Some(window) => format!("{} / {} tokens", render::tokens(used), render::tokens(u64::from(window))),
            None => format!("{} tokens", render::tokens(used)),
        });
        self.after_remote(std::iter::once(self.model.clone()).chain(thinking).chain(context))
    }

    fn after_remote(&self, parts: impl Iterator<Item = String>) -> String {
        self.remote.iter().cloned().chain(parts).collect::<Vec<_>>().join(" · ")
    }
}

/// The model without its vendor or Ollama's default tag, which `/model` still
/// shows in full.
fn short_name(reference: &str) -> &str {
    let name = reference.rsplit_once('/').map_or(reference, |(_, name)| name);
    name.strip_suffix(":latest").unwrap_or(name)
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub enum Read {
    /// Enter: what was typed, still shown above the cursor.
    Line(String),
    /// Ctrl+Q: what was typed, to send once the answer in progress ends.
    Queue(String),
    /// Ctrl+S: what was typed, to send at once, stopping the answer.
    Now(String),
    /// Alt+Up: the message queued last, back to edit.
    Unqueue,
    /// Ctrl+C, with what was typed.
    Interrupted(String),
    /// Something outside the editor needs the terminal. What was typed
    /// stays, for the next read.
    Woken,
    Exit,
}

/// What keys bound to [`ReedlineEvent::ExecuteHostCommand`] ask for.
const QUEUE: &str = "queue";
const NOW: &str = "now";
const UNQUEUE: &str = "unqueue";

pub struct Editor {
    line_editor: Reedline,
    prompt: HeyPrompt,
    /// Breaks a read, for [`Read::Woken`].
    wake: Arc<AtomicBool>,
    repaint: RepaintSignal,
}

impl Editor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Editor,
        colors: bool,
        status: SharedStatus,
        outbox: Outbox,
        choices: Choices,
        index: &Index,
        watch: Watch,
        wake: Arc<AtomicBool>,
    ) -> Self {
        let completer = InputCompleter {
            choices,
            mentions: Mentions::new(index.clone()),
            index: index.clone(),
            matcher: SkimMatcherV2::default(),
        };
        let mut line_editor = Reedline::create()
            .with_edit_mode(edit_mode(config.mode))
            .with_completer(Box::new(completer))
            .with_transient_prompt(Box::new(HeyPrompt::Submitted))
            .with_highlighter(Box::new(InputHighlighter(index.clone())))
            .with_status_line(Box::new(InputHelp(index.clone())))
            .with_buffer_editor(buffer_editor(), std::env::temp_dir().join("hey-prompt.md"))
            .with_history_exclusion_prefix(Some(" ".into()))
            .with_ansi_colors(colors)
            .with_cursor_config(CursorConfig {
                vi_insert: Some(SetCursorStyle::BlinkingBar),
                vi_normal: Some(SetCursorStyle::BlinkingBlock),
                hx_insert: Some(SetCursorStyle::BlinkingBar),
                hx_normal: Some(SetCursorStyle::BlinkingBlock),
                hx_select: Some(SetCursorStyle::BlinkingUnderScore),
                ..CursorConfig::default()
            })
            .use_kitty_keyboard_enhancement(true)
            .use_bracketed_paste(config.bracketed_paste)
            .with_break_signal(wake.clone())
            .with_external_printer(screen::Feed)
            .with_poll_interval(POLL);

        if config.history
            && let Some(path) = config::config_dir().map(|dir| dir.join("history.txt"))
        {
            let history = std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .map_err(reedline::ReedlineError::from)
                .and_then(|()| FileBackedHistory::with_file(config.history_size, path));
            match history {
                Ok(history) => line_editor = line_editor.with_history(Box::new(history)),
                Err(error) => render::notice(&format!("input history disabled: {error}")),
            }
        }
        let repaint = line_editor.repaint_signal();
        Self {
            line_editor: styled(line_editor, palette()),
            prompt: HeyPrompt::Editing {
                status,
                outbox,
                watch,
                colors,
                right: config.right_prompt,
                above: Cell::new(0),
            },
            wake,
            repaint,
        }
    }

    /// Redraws the menu and hints in the current theme's colours.
    pub fn restyle(&mut self) {
        // Reedline only takes these styles by value, as it is built.
        let line_editor = std::mem::replace(&mut self.line_editor, Reedline::create());
        self.line_editor = styled(line_editor, palette());
    }

    /// Redraws the prompt in place while a line is being read.
    pub fn repaint_signal(&self) -> RepaintSignal {
        self.repaint.clone()
    }

    /// Ends a read in progress with [`Read::Woken`].
    pub fn waker(&self) -> Arc<AtomicBool> {
        self.wake.clone()
    }

    /// Reads a line, or a key that sends one another way. A read that
    /// breaks off leaves the prompt where it is, to be drawn again in place
    /// by the next; Ctrl+C takes it off the screen.
    pub fn read(&mut self) -> Read {
        match self.line_editor.read_line(&self.prompt) {
            Ok(Signal::Success(line)) => Read::Line(line),
            Ok(Signal::HostCommand(command)) => self.host_command(&command),
            Ok(Signal::ExternalBreak(_)) => Read::Woken,
            Ok(Signal::CtrlC(typed)) => {
                erase(self.above() + rows(&typed, render::terminal_width().max(1)));
                Read::Interrupted(typed)
            }
            Ok(_) => Read::Exit,
            Err(error) => {
                render::error(&error);
                Read::Exit
            }
        }
    }

    /// Takes `line`, submitted with Enter, off the screen.
    pub fn unprint(&self, line: &str) {
        erase(rows(line, render::terminal_width().max(1)));
    }

    /// Puts `text` in the input, before what is typed there.
    pub fn insert(&mut self, text: &str) {
        let commands = match self.line_editor.current_buffer_contents().is_empty() {
            true => vec![EditCommand::InsertString(text.to_owned())],
            false => vec![
                EditCommand::MoveToStart { select: false },
                EditCommand::InsertString(format!("{text}\n")),
                EditCommand::MoveToEnd { select: false },
            ],
        };
        self.line_editor.run_edit_commands(&commands);
    }

    /// What a key sending the input another way asks for, the input taken
    /// and kept in history as a submitted line would be.
    fn host_command(&mut self, command: &str) -> Read {
        let send = match command {
            QUEUE => Read::Queue,
            NOW => Read::Now,
            UNQUEUE => return Read::Unqueue,
            _ => return Read::Woken,
        };
        let typed = self.line_editor.current_buffer_contents().to_owned();
        self.line_editor.run_edit_commands(&[EditCommand::Clear]);
        if !typed.trim().is_empty() && !typed.starts_with(' ') {
            let saved = self.line_editor.history_mut().save(HistoryItem::from_command_line(&typed));
            if let Err(error) = saved {
                render::notice(&format!("not kept in history: {error}"));
            }
        }
        send(typed)
    }

    /// Takes the prompt off the screen after a read that broke off, leaving
    /// the cursor where it began, which the next read draws it from again.
    /// What is drawn meanwhile must leave the cursor there too.
    pub fn erase_prompt(&self) {
        let width = render::terminal_width().max(1);
        let typed = self.line_editor.current_buffer_contents();
        let before = typed.get(..self.line_editor.current_insertion_point()).unwrap_or(typed);
        let (lines, last) = before.rsplit_once('\n').map_or((None, before), |(lines, last)| (Some(lines), last));
        let above_cursor = self.above() + lines.map_or(0, |lines| rows(lines, width));
        erase(above_cursor + (INDICATOR + display_width(last)) / width);
    }

    /// Rows the prompt showed above the input when last drawn.
    fn above(&self) -> usize {
        match &self.prompt {
            HeyPrompt::Editing { above, .. } => above.get(),
            HeyPrompt::Submitted => 0,
        }
    }

    pub fn clear_screen(&mut self) {
        if let Err(error) = self.line_editor.clear_screen() {
            render::error(&error);
        }
    }
}

/// Rows `text` takes as typed after the prompt indicator, each of its lines
/// starting a row and wrapping at `width` columns.
fn rows(text: &str, width: usize) -> usize {
    text.split('\n')
        .map(|line| (INDICATOR + display_width(line)).div_ceil(width).max(1))
        .sum()
}

/// Clears the cursor's row and those below, and the `up` rows above it,
/// leaving the cursor where the first of them began.
fn erase(up: usize) {
    let mut out = std::io::stdout();
    let _ = match u16::try_from(up) {
        Ok(0) => execute!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown)),
        up => execute!(out, MoveToColumn(0), MoveUp(up.unwrap_or(u16::MAX)), Clear(ClearType::FromCursorDown)),
    };
}

/// The parts of `line_editor` that keep their colours: the completion menu,
/// history hints and the selection.
fn styled(line_editor: Reedline, palette: Palette) -> Reedline {
    line_editor
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu(palette))))
        .with_hinter(Box::new(MessageHinter {
            history: DefaultHinter::default().with_style(fg(palette.muted)),
            command: false,
        }))
        .with_visual_selection_style(Style::new().reverse())
}

/// Nushell's default completion menu: plain columns, or one suggestion per row
/// with its description alongside.
fn completion_menu(palette: Palette) -> ColumnarMenu {
    ColumnarMenu::default()
        .with_name(MENU)
        .with_columns(4)
        .with_column_width(Some(20))
        .with_column_padding(2)
        .with_text_style(fg(palette.name))
        .with_selected_text_style(fg(palette.name).reverse())
        .with_description_text_style(fg(palette.muted))
        .with_match_text_style(fg(palette.name).underline())
        .with_selected_match_text_style(fg(palette.name).underline().reverse())
}

fn fg(color: render::Color) -> Style {
    Style::new().fg(color.into())
}

impl From<render::Color> for Color {
    fn from(color: render::Color) -> Self {
        match color {
            render::Color::Default => Self::Default,
            render::Color::Ansi(index) => Self::Fixed(index),
            render::Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

fn edit_mode(mode: Mode) -> Box<dyn EditMode> {
    match mode {
        Mode::Emacs => Box::new(Emacs::new(with_bindings(default_emacs_keybindings()))),
        Mode::Vi => Box::new(Vi::new(
            with_bindings(default_vi_insert_keybindings()),
            with_menu_bindings(default_vi_normal_keybindings()),
            default_vi_visual_keybindings(),
        )),
        Mode::Helix => Box::new(
            Helix::default()
                .with_insert_keybindings(with_bindings(default_helix_insert_keybindings()))
                .with_normal_keybindings(with_menu_bindings(default_helix_normal_keybindings())),
        ),
    }
}

/// Bindings for modes that insert text: menus, plus `/` and `@` opening the
/// completion menu, Ctrl+R searching history and Ctrl+O composing in `$EDITOR`.
fn with_bindings(keybindings: Keybindings) -> Keybindings {
    let mut keybindings = with_menu_bindings(keybindings);
    for sigil in ['/', '@'] {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(sigil),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(sigil)]),
                ReedlineEvent::Menu(MENU.into()),
            ]),
        );
    }
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('r'),
        ReedlineEvent::SearchHistory,
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('o'),
        ReedlineEvent::OpenEditor,
    );
    keybindings
}

fn with_menu_bindings(mut keybindings: Keybindings) -> Keybindings {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Menu(MENU.into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    for (modifiers, key, command) in [
        (KeyModifiers::CONTROL, KeyCode::Char('q'), QUEUE),
        (KeyModifiers::CONTROL, KeyCode::Char('s'), NOW),
        (KeyModifiers::ALT, KeyCode::Up, UNQUEUE),
    ] {
        keybindings.add_binding(modifiers, key, ReedlineEvent::ExecuteHostCommand(command.into()));
    }
    keybindings
}

/// History hints on messages only. Tab takes a hint before opening the menu,
/// and on a command line the commands and their arguments are the better
/// completion.
struct MessageHinter {
    history: DefaultHinter,
    command: bool,
}

impl Hinter for MessageHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        self.command = line.starts_with('/');
        match self.command {
            true => String::new(),
            false => self.history.handle(line, pos, history, use_ansi_coloring, cwd),
        }
    }

    fn complete_hint(&self) -> String {
        match self.command {
            true => String::new(),
            false => self.history.complete_hint(),
        }
    }

    fn next_hint_token(&self) -> String {
        match self.command {
            true => String::new(),
            false => self.history.next_hint_token(),
        }
    }

    fn continues_text_before_cursor(&self) -> bool {
        self.history.continues_text_before_cursor()
    }
}

/// `$VISUAL`, else `$EDITOR`, else `vi`, split into program and arguments so
/// values like `code --wait` work.
fn buffer_editor() -> std::process::Command {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut words = editor.split_whitespace();
    let mut command = std::process::Command::new(words.next().unwrap_or("vi"));
    command.args(words);
    command
}

/// What slash commands and their arguments complete to.
pub struct Choices {
    pub models: Vec<String>,
    pub thinking: Vec<String>,
    pub themes: Vec<String>,
}

impl Choices {
    /// Commands and the host's skills after `/`, then a command's argument.
    fn suggestions(&self, before: &str, index: &HostIndex, matcher: &SkimMatcherV2) -> Vec<Suggestion> {
        let Some(typed) = before.strip_prefix('/') else {
            return Vec::new();
        };
        let Some((word, arg)) = typed.split_once(' ') else {
            let span = Span::new(0, before.len());
            let commands = COMMANDS
                .iter()
                .filter(|spec| spec.name.starts_with(typed))
                .map(|spec| Suggestion {
                    value: format!("/{}", spec.name),
                    description: Some(spec.about.into()),
                    span,
                    append_whitespace: spec.arg.is_some(),
                    ..Suggestion::default()
                });
            let invocable = index.skills.skills.iter().filter(|skill| index.command(skill.name.as_str()).is_some());
            let skills = host::skill_suggestions(matcher, invocable, typed).map(|(skill, indices)| Suggestion {
                value: format!("/{}", skill.name),
                ..host::skill_suggestion(skill, indices, span)
            });
            return commands.chain(skills).collect();
        };
        let start = before.len() - arg.len();
        let span = Span::new(start, before.len());
        let values = match commands::find(word).and_then(|spec| spec.arg) {
            Some(Arg::Model) => &self.models,
            Some(Arg::Thinking) => &self.thinking,
            Some(Arg::Theme) => &self.themes,
            Some(Arg::Thread) => {
                return host::thread_matches(matcher, &index.threads, arg.trim())
                    .map(|summary| host::thread_suggestion(summary, span))
                    .collect();
            }
            Some(Arg::Focus) | None => return Vec::new(),
        };
        values
            .iter()
            .filter(|value| value.contains(arg.trim()))
            .map(|value| Suggestion {
                value: value.clone(),
                span,
                ..Suggestion::default()
            })
            .collect()
    }
}

/// Slash commands, skills and command arguments on a command line, `@`
/// mentions on a message.
struct InputCompleter {
    choices: Choices,
    mentions: Mentions,
    index: Index,
    matcher: SkimMatcherV2,
}

impl Completer for InputCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let before = line.get(..pos).unwrap_or(line);
        let suggestions = match before.starts_with('/') {
            true => self.choices.suggestions(before, &host::lock(&self.index), &self.matcher),
            false => self.mentions.suggestions(before),
        };
        CompletionResult::fresh(suggestions)
    }
}

/// Messages in the input colour with the mentions that name something picked
/// out; commands and their arguments as their roles.
struct InputHighlighter(Index);

impl Highlighter for InputHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let palette = palette();
        let mut styled = StyledText::new();
        match line.strip_prefix('/') {
            None => {
                let message = fg(palette.input);
                let mut last = 0;
                let index = host::lock(&self.0);
                let known = mention::mentions(line)
                    .filter_map(|mention| mention.resolve(|reference| index.describe(reference)));
                for (_, span) in known {
                    styled.push((message, line[last..span.start].to_owned()));
                    styled.push((fg(palette.name), line[span.clone()].to_owned()));
                    last = span.end;
                }
                styled.push((message, line[last..].to_owned()));
            }
            Some(rest) => {
                let (word, arg) = rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
                let known = commands::find(word).is_some() || host::lock(&self.0).command(word).is_some();
                let word_style = match known {
                    true => fg(palette.name),
                    false => Color::Yellow.normal(),
                };
                styled.push((Style::default(), "/".into()));
                styled.push((word_style, word.to_owned()));
                styled.push((fg(palette.value), arg.to_owned()));
            }
        }
        styled
    }
}

/// Below the line while typing a slash command or a mention: what it does, or
/// why it is unknown.
struct InputHelp(Index);

impl StatusLine for InputHelp {
    fn status(&mut self, line: &str, pos: usize, columns: u16, colors: bool) -> String {
        let (style, text) = match line.trim_start().strip_prefix('/') {
            Some(typed) => self.command_help(typed),
            None => match mention::under(line, pos) {
                Some(mention) => self.mention_help(&mention),
                None => return String::new(),
            },
        };
        let text: String = text.chars().take(usize::from(columns)).collect();
        if colors { style.paint(text).to_string() } else { text }
    }
}

impl InputHelp {
    fn mention_help(&self, mention: &Mention) -> (Style, String) {
        let muted = fg(palette().muted);
        let index = host::lock(&self.0);
        match mention.resolve(|reference| index.describe(reference)) {
            Some((attached, _)) => (muted, format!("attaches {attached}")),
            None => match mention.reference() {
                Reference::Local("") => (muted, "Tab for files and directories".into()),
                Reference::Shared("") => (muted, "Tab for the host's shared files".into()),
                Reference::Thread("") => (muted, "Tab for saved conversations".into()),
                Reference::Skill("") => (muted, "Tab for skills".into()),
                _ => (
                    Color::Yellow.normal(),
                    format!("nothing named {}; sent as typed", mention.text),
                ),
            },
        }
    }

    fn command_help(&self, typed: &str) -> (Style, String) {
        let muted = fg(palette().muted);
        let word = typed.split_whitespace().next().unwrap_or_default();
        if let Some(spec) = commands::find(word) {
            return (muted, format!("{}  {}", spec.usage(), spec.about));
        }
        match host::lock(&self.0).command(word) {
            Some(skill) => (muted, format!("/{} [message]  {}", skill.name, skill.description)),
            None if word.is_empty() => (muted, "Tab for commands and skills".into()),
            None => (Color::Yellow.normal(), format!("unknown command /{word}")),
        }
    }
}

/// Reedline weighs the input against the right prompt on the left prompt's
/// last line, and an empty one has none. This SGR reset is a line yet takes
/// no columns, so the status hides once the input reaches it rather than
/// being typed over.
const LEFT: &str = "\x1b[0m";

/// While editing: what an answer in progress redraws and the messages
/// waiting for it, above a bare indicator, and the status on the right edge
/// of the input's row, as in nushell; reedline drops the status once the
/// input reaches it. The status starts with the other threads that are
/// answering or answered unseen.
enum HeyPrompt {
    Editing {
        status: SharedStatus,
        outbox: Outbox,
        watch: Watch,
        colors: bool,
        right: RightPrompt,
        /// Rows drawn above the input last time, to erase them.
        above: Cell<usize>,
    },
    /// How a submitted line stays in the scrollback: without the status or
    /// anything above it.
    Submitted,
}

/// The editing status: a spinner and a blue dot counting the other threads
/// answering and answered unseen, then the model and its context.
fn status_line(
    status: &SharedStatus,
    watch: &Watch,
    colors: bool,
    summary: fn(&Status) -> String,
) -> String {
    let paint = |color: Color, text: &str| match colors {
        true => color.paint(text).to_string(),
        false => text.to_owned(),
    };
    let muted = |text: &str| paint(palette().muted.into(), text);
    let elsewhere = watch.elsewhere();
    let running = (elsewhere.running > 0).then(|| {
        let spinner = paint(palette().accent.into(), watch::spinner());
        format!("{spinner}{}", muted(&format!(" {} running", elsewhere.running)))
    });
    let unseen = (elsewhere.unseen > 0).then(|| {
        let dot = paint(Color::Blue, "●");
        format!("{dot}{}", muted(&format!(" {} new", elsewhere.unseen)))
    });
    let summary = summary(&status.lock().unwrap_or_else(PoisonError::into_inner));
    running
        .into_iter()
        .chain(unseen)
        .chain(std::iter::once(muted(&summary)))
        .collect::<Vec<_>>()
        .join(&muted(" · "))
}

impl Prompt for HeyPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let Self::Editing { outbox, above, .. } = self else {
            return Cow::Borrowed(LEFT);
        };
        // Each row from a reset, against the prompt colour reedline sets.
        let rows: Vec<String> = screen::live()
            .into_iter()
            .chain(outbox.rows(render::terminal_width().saturating_sub(1)))
            .map(|row| format!("{LEFT}{row}"))
            .collect();
        above.set(rows.len());
        match rows.is_empty() {
            true => Cow::Borrowed(LEFT),
            false => Cow::Owned(format!("{}\n{LEFT}", rows.join("\n"))),
        }
    }

    fn right_prompt_on_last_line(&self) -> bool {
        true
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        match self {
            Self::Editing {
                status,
                watch,
                colors,
                right,
                ..
            } => {
                let summary = match right {
                    RightPrompt::Off => return Cow::Borrowed(""),
                    RightPrompt::Short => Status::short,
                    RightPrompt::Full => Status::full,
                };
                Cow::Owned(status_line(status, watch, *colors, summary))
            }
            Self::Submitted => Cow::Borrowed(""),
        }
    }

    fn render_prompt_indicator(&self, mode: PromptEditMode) -> Cow<'_, str> {
        match mode {
            PromptEditMode::Vi(PromptViMode::Normal | PromptViMode::Visual)
            | PromptEditMode::Helix(PromptHelixMode::Normal | PromptHelixMode::Select) => {
                Cow::Borrowed(": ")
            }
            _ => Cow::Borrowed("> "),
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        match search.status {
            PromptHistorySearchStatus::Passing => Cow::Borrowed("? "),
            PromptHistorySearchStatus::Failing => Cow::Borrowed("?! "),
        }
    }

    fn get_prompt_color(&self) -> Color {
        palette().accent.into()
    }

    fn get_indicator_color(&self) -> Color {
        palette().accent.into()
    }

    fn get_prompt_right_color(&self) -> Color {
        palette().muted.into()
    }
}
