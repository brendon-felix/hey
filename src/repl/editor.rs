/* -------------------------------------------------------------------------- */
/*                               repl/editor.rs                               */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use crossterm::cursor::{MoveToColumn, SetCursorStyle};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, Completer, CompletionResult, CursorConfig, DefaultHinter, EditCommand,
    EditMode, Emacs, FileBackedHistory, Helix, Highlighter, KeyCode, KeyModifiers,
    Keybindings, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, PromptHelixMode, PromptViMode, Reedline, ReedlineEvent,
    ReedlineMenu, RepaintSignal, Signal, Span, StatusLine, StyledText, Suggestion, Vi,
    default_emacs_keybindings,
    default_helix_insert_keybindings, default_helix_normal_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
};

use super::commands::{self, Arg, COMMANDS};
use super::mentions::{self as host, Index, Mentions};
use super::watch::{self, Watch};
use crate::config::{self, EditMode as Mode, Thinking};
use crate::mention::{self, Mention, Reference};
use crate::render::{self, Palette, palette};

const MENU: &str = "completion_menu";

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
    fn summary(&self) -> String {
        let name = short_name(&self.model);
        let model = match self.thinking {
            Some(level) => format!("{name} {}", level.as_str()),
            None => name.to_owned(),
        };
        let context = self.context.map(|used| match self.window {
            Some(window) => format!("{}/{}", compact(used), compact(u64::from(window))),
            None => compact(used),
        });
        self.remote
            .iter()
            .cloned()
            .chain(std::iter::once(model))
            .chain(context)
            .collect::<Vec<_>>()
            .join(" · ")
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
    Line(String),
    /// A run started on this terminal's thread elsewhere. What was typed
    /// stays, for the next read.
    Woken,
    Exit,
}

pub struct Editor {
    line_editor: Reedline,
    prompt: HeyPrompt,
}

impl Editor {
    pub fn new(
        config: &config::Editor,
        colors: bool,
        status: SharedStatus,
        choices: Choices,
        index: &Index,
        watch: Watch,
        wake: Arc<AtomicBool>,
    ) -> Self {
        let completer = InputCompleter {
            choices,
            mentions: Mentions::new(index.clone()),
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
            .with_break_signal(wake);

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
        Self {
            line_editor: styled(line_editor, palette()),
            prompt: HeyPrompt::Editing {
                status,
                watch,
                colors,
            },
        }
    }

    /// Redraws the menu and hints in the current theme's colours.
    pub fn restyle(&mut self) {
        // Reedline only takes these styles by value, as it is built.
        let line_editor = std::mem::replace(&mut self.line_editor, Reedline::create());
        self.line_editor = styled(line_editor, palette());
    }

    /// Redraws the prompt in place while a line is being read.
    pub fn repaint_signal(&mut self) -> RepaintSignal {
        self.line_editor.repaint_signal()
    }

    pub fn read(&mut self) -> Read {
        match self.line_editor.read_line(&self.prompt) {
            Ok(Signal::Success(line)) => Read::Line(line),
            Ok(Signal::ExternalBreak(_)) => {
                // The prompt is redrawn below whatever is shown next.
                let _ = execute!(
                    std::io::stdout(),
                    MoveToColumn(0),
                    Clear(ClearType::FromCursorDown)
                );
                Read::Woken
            }
            Ok(_) => Read::Exit,
            Err(error) => {
                render::error(&error);
                Read::Exit
            }
        }
    }

    pub fn clear_screen(&mut self) {
        if let Err(error) = self.line_editor.clear_screen() {
            render::error(&error);
        }
    }
}

/// The parts of `line_editor` that keep their colours: the completion menu,
/// history hints and the selection.
fn styled(line_editor: Reedline, palette: Palette) -> Reedline {
    line_editor
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu(palette))))
        .with_hinter(Box::new(DefaultHinter::default().with_style(fg(palette.muted))))
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
    keybindings
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
    fn suggestions(&self, before: &str) -> Vec<Suggestion> {
        let Some(typed) = before.strip_prefix('/') else {
            return Vec::new();
        };
        let Some((word, arg)) = typed.split_once(' ') else {
            return COMMANDS
                .iter()
                .filter(|spec| spec.name.starts_with(typed))
                .map(|spec| Suggestion {
                    value: format!("/{}", spec.name),
                    description: Some(spec.about.into()),
                    span: Span::new(0, before.len()),
                    append_whitespace: spec.arg.is_some(),
                    ..Suggestion::default()
                })
                .collect();
        };
        let values = match commands::find(word).and_then(|spec| spec.arg) {
            Some(Arg::Model) => &self.models,
            Some(Arg::Thinking) => &self.thinking,
            Some(Arg::Theme) => &self.themes,
            None => return Vec::new(),
        };
        let start = before.len() - arg.len();
        values
            .iter()
            .filter(|value| value.contains(arg.trim()))
            .map(|value| Suggestion {
                value: value.clone(),
                span: Span::new(start, before.len()),
                ..Suggestion::default()
            })
            .collect()
    }
}

/// Slash commands and their arguments on a command line, `@` mentions on a
/// message.
struct InputCompleter {
    choices: Choices,
    mentions: Mentions,
}

impl Completer for InputCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let before = line.get(..pos).unwrap_or(line);
        let suggestions = match before.starts_with('/') {
            true => self.choices.suggestions(before),
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
                let word_style = match commands::find(word) {
                    Some(_) => fg(palette.name),
                    None => Color::Yellow.normal(),
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
            Some(typed) => command_help(typed),
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
                _ => (
                    Color::Yellow.normal(),
                    format!("nothing named {}; sent as typed", mention.text),
                ),
            },
        }
    }
}

fn command_help(typed: &str) -> (Style, String) {
    let muted = fg(palette().muted);
    let word = typed.split_whitespace().next().unwrap_or_default();
    match commands::find(word) {
        Some(spec) => (muted, format!("{}  {}", spec.usage(), spec.about)),
        None if word.is_empty() => (muted, "Tab for commands".into()),
        None => (Color::Yellow.normal(), format!("unknown command /{word}")),
    }
}

/// A token count in at most four columns: a tenth only below ten, as in
/// `9.5k`, `24k` and `1M`.
fn compact(tokens: u64) -> String {
    let (value, unit) = match tokens {
        0..1_000 => return tokens.to_string(),
        1_000..999_500 => (tokens as f64 / 1e3, "k"),
        _ => (tokens as f64 / 1e6, "M"),
    };
    match value < 9.95 {
        true => format!("{value:.1}").trim_end_matches(".0").to_owned() + unit,
        false => format!("{value:.0}{unit}"),
    }
}

/// Reedline weighs the input against the right prompt only when the left prompt
/// spans exactly one line, and an empty one spans none. This SGR reset is one
/// line yet takes no columns, so the status hides once the input reaches it
/// rather than being typed over.
const LEFT: &str = "\x1b[0m";

/// A bare indicator on the left and, while editing, the status on the right
/// edge, as in nushell; reedline drops the right side once the input reaches
/// it. The status starts with the other threads that are answering or
/// answered unseen.
enum HeyPrompt {
    Editing {
        status: SharedStatus,
        watch: Watch,
        colors: bool,
    },
    /// How a submitted line stays in the scrollback: without the status.
    Submitted,
}

/// The editing status: a spinner and a blue dot counting the other threads
/// answering and answered unseen, then the model and its context.
fn status_line(status: &SharedStatus, watch: &Watch, colors: bool) -> String {
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
    let summary = status.lock().unwrap_or_else(PoisonError::into_inner).summary();
    running
        .into_iter()
        .chain(unseen)
        .chain(std::iter::once(muted(&summary)))
        .collect::<Vec<_>>()
        .join(&muted(" · "))
}

impl Prompt for HeyPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(LEFT)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        match self {
            Self::Editing {
                status,
                watch,
                colors,
            } => Cow::Owned(status_line(status, watch, *colors)),
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
