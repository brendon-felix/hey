/* -------------------------------------------------------------------------- */
/*                               repl/editor.rs                               */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;
use std::sync::{Arc, Mutex, PoisonError};

use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color, Style};
use reedline::{
    Completer, CompletionResult, CursorConfig, DefaultHinter, DescriptionMode, EditCommand,
    EditMode, Emacs, FileBackedHistory, Helix, Highlighter, IdeMenu, KeyCode, KeyModifiers,
    Keybindings, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    PromptHistorySearchStatus, PromptHelixMode, PromptViMode, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Span, StatusLine, StyledText, Suggestion, Vi, default_emacs_keybindings,
    default_helix_insert_keybindings, default_helix_normal_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
};

use super::commands::{self, Arg, COMMANDS};
use crate::config::{self, EditMode as Mode, Thinking};
use crate::render;

const MENU: &str = "completion_menu";

/// What the status line shows when the line is not a command.
#[derive(Debug, Default)]
pub struct Status {
    pub model: String,
    pub thinking: Option<Thinking>,
    pub context: Option<u64>,
    pub window: Option<u32>,
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub enum Read {
    Line(String),
    Cancelled,
    Exit,
}

pub struct Editor {
    line_editor: Reedline,
}

impl Editor {
    pub fn new(
        config: &config::Editor,
        colors: bool,
        status: SharedStatus,
        choices: Choices,
    ) -> Self {
        let menu = IdeMenu::default()
            .with_name(MENU)
            .with_default_border()
            .with_description_mode(DescriptionMode::PreferRight);
        let mut line_editor = Reedline::create()
            .with_edit_mode(edit_mode(config.mode))
            .with_completer(Box::new(choices))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
            .with_highlighter(Box::new(InputHighlighter))
            .with_status_line(Box::new(StatusBar(status)))
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
            .use_bracketed_paste(config.bracketed_paste);

        if config.history
            && let Some(path) = config::config_dir().map(|dir| dir.join("history.txt"))
        {
            let history = std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .map_err(reedline::ReedlineError::from)
                .and_then(|()| FileBackedHistory::with_file(config.history_size, path));
            match history {
                Ok(history) => {
                    let hint = Style::new().fg(Color::DarkGray);
                    line_editor = line_editor
                        .with_history(Box::new(history))
                        .with_hinter(Box::new(DefaultHinter::default().with_style(hint)));
                }
                Err(error) => render::notice(&format!("input history disabled: {error}")),
            }
        }
        Self { line_editor }
    }

    pub fn read(&mut self) -> Read {
        match self.line_editor.read_line(&HeyPrompt) {
            Ok(Signal::Success(line)) => Read::Line(line),
            Ok(Signal::CtrlD) => Read::Exit,
            Ok(_) => Read::Cancelled,
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

/// Bindings for modes that insert text: menus, plus `/` opening the command
/// menu, Ctrl+R searching history and Ctrl+O composing in `$EDITOR`.
fn with_bindings(keybindings: Keybindings) -> Keybindings {
    let mut keybindings = with_menu_bindings(keybindings);
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('/'),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
            ReedlineEvent::Menu(MENU.into()),
        ]),
    );
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

impl Completer for Choices {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let before = line.get(..pos).unwrap_or(line);
        CompletionResult::fresh(self.suggestions(before))
    }
}

struct InputHighlighter;

impl Highlighter for InputHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        match line.strip_prefix('/') {
            None => styled.push((Style::new().fg(Color::Green), line.to_owned())),
            Some(rest) => {
                let (word, arg) = rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
                let known = commands::find(word).is_some();
                let word_color = if known { Color::Cyan } else { Color::Yellow };
                styled.push((Style::default(), "/".into()));
                styled.push((Style::new().fg(word_color), word.to_owned()));
                styled.push((Style::new().fg(Color::Blue), arg.to_owned()));
            }
        }
        styled
    }
}

/// Below the line: help for the command being typed, else the model and how
/// full its context is.
struct StatusBar(SharedStatus);

impl StatusLine for StatusBar {
    fn status(&mut self, line: &str, _pos: usize, columns: u16, colors: bool) -> String {
        let dim = Style::new().fg(Color::DarkGray);
        let (style, text) = match line.trim_start().strip_prefix('/') {
            Some(typed) => {
                let word = typed.split_whitespace().next().unwrap_or_default();
                match commands::find(word) {
                    Some(spec) => (dim, format!("{}  {}", spec.usage(), spec.about)),
                    None if word.is_empty() => (dim, "Tab for commands".into()),
                    None => (Style::new().fg(Color::Yellow), format!("unknown command /{word}")),
                }
            }
            None => (dim, self.describe()),
        };
        let text: String = text.chars().take(usize::from(columns)).collect();
        if colors { style.paint(text).to_string() } else { text }
    }
}

impl StatusBar {
    fn describe(&self) -> String {
        let status = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let thinking = status
            .thinking
            .map(|level| format!("thinking {}", level.as_str()));
        let context = match (status.context, status.window) {
            (Some(used), Some(window)) => {
                Some(format!("{} / {} tokens", compact(used), compact(u64::from(window))))
            }
            (Some(used), None) => Some(format!("{} tokens", compact(used))),
            (None, _) => None,
        };
        std::iter::once(status.model.clone())
            .chain(thinking)
            .chain(context)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn compact(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1e3),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

struct HeyPrompt;

impl Prompt for HeyPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
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

    fn get_prompt_color(&self) -> reedline::Color {
        reedline::Color::Green
    }

    fn get_indicator_color(&self) -> reedline::Color {
        reedline::Color::Magenta
    }
}
