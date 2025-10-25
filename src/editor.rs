/* -------------------------------------------------------------------------- */
/*                                 editor.rs                                  */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;
use std::path::PathBuf;

use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color as NuColor, Style};
use reedline::{
    Color, ColumnarMenu, Completer, CursorConfig, DefaultHinter, EditCommand, EditMode, Emacs,
    FileBackedHistory, Highlighter, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, StyledText,
    Suggestion, ValidationResult, Validator, Vi, default_emacs_keybindings,
    default_vi_insert_keybindings, default_vi_normal_keybindings,
};

use crate::{
    commands::{Command, get_completion_strings, parse_command},
    config::Config,
};

pub enum Input {
    Message(String),
    Command(Command),
    Invalid,
}

enum ReedlineEditMode {
    Emacs,
    Vi,
}

pub struct EditorConfig {
    edit_mode: ReedlineEditMode,
    use_kitty_keyboard_enhancement: bool,
    use_bracketed_paste: bool,
    ansi_colors: bool,
    history_file_path: Option<PathBuf>,
    history_max_size: usize,
}

impl EditorConfig {
    pub fn from_config(config: &Config) -> Self {
        let edit_mode = match config.edit_mode.to_lowercase().as_str() {
            "vi" | "vim" => ReedlineEditMode::Vi,
            "emacs" | "default" => ReedlineEditMode::Emacs,
            _ => {
                println!(
                    "Warning: Unknown edit mode '{}', defaulting to 'emacs'",
                    config.edit_mode
                );
                ReedlineEditMode::Emacs
            }
        };

        let history_file_path = if config.reedline_history {
            crate::config::get_history_file_path().ok()
        } else {
            None
        };

        EditorConfig {
            edit_mode,
            use_kitty_keyboard_enhancement: true,
            use_bracketed_paste: config.bracketed_paste,
            ansi_colors: config.ansi_colors,
            history_file_path,
            history_max_size: config.history_max_size,
        }
    }
}

#[derive(Clone)]
struct CommandCompleter {
    commands: Vec<String>,
}

impl CommandCompleter {
    pub fn new(commands: Vec<String>) -> Self {
        CommandCompleter { commands }
    }
}

impl Completer for CommandCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let line = &line[..pos.min(line.len())];
        if !line.starts_with('/') {
            return Vec::new();
        }
        let start = pos - line.len();
        self.commands
            .iter()
            .filter(|cmd| cmd.starts_with(line))
            .map(|cmd| Suggestion {
                value: cmd.clone(),
                span: Span::new(start, pos),
                ..Default::default()
            })
            .collect()
    }
}

pub struct Editor {
    line_editor: Reedline,
    prompt: EditorPrompt,
}

impl Editor {
    pub fn new(editor_config: EditorConfig) -> Self {
        let cmd_completer = Box::new(CommandCompleter::new(get_completion_strings()));
        let cmd_completion_menu = Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_columns(1),
        );
        let slash_binding = ReedlineEvent::Multiple(vec![
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
            ReedlineEvent::Menu("completion_menu".to_string()),
        ]);
        let tab_binding = ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::MenuNext,
        ]);
        let shift_tab_binding = ReedlineEvent::MenuPrevious;
        let history_binding = ReedlineEvent::SearchHistory;

        let edit_mode: Box<dyn EditMode> = match editor_config.edit_mode {
            ReedlineEditMode::Vi => {
                let mut insert = default_vi_insert_keybindings();
                let mut normal = default_vi_normal_keybindings();
                insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_binding.clone());
                insert.add_binding(KeyModifiers::SHIFT, KeyCode::Tab, shift_tab_binding.clone());
                insert.add_binding(KeyModifiers::NONE, KeyCode::Char('/'), slash_binding);
                insert.add_binding(KeyModifiers::CONTROL, KeyCode::Char('r'), history_binding);
                normal.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_binding);
                normal.add_binding(KeyModifiers::SHIFT, KeyCode::Tab, shift_tab_binding);
                Box::new(Vi::new(insert, normal))
            }
            ReedlineEditMode::Emacs => {
                let mut keybindings = default_emacs_keybindings();
                keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_binding);
                keybindings.add_binding(KeyModifiers::SHIFT, KeyCode::Tab, shift_tab_binding);
                keybindings.add_binding(KeyModifiers::NONE, KeyCode::Char('/'), slash_binding);
                keybindings.add_binding(KeyModifiers::CONTROL, KeyCode::Char('r'), history_binding);
                Box::new(Emacs::new(keybindings))
            }
        };

        let mut line_editor = Reedline::create()
            .with_edit_mode(edit_mode)
            .with_menu(ReedlineMenu::WithCompleter {
                menu: cmd_completion_menu,
                completer: cmd_completer,
            })
            .with_highlighter(Box::new(PromptHighlighter::new()))
            .with_ansi_colors(editor_config.ansi_colors)
            .with_validator(Box::new(PromptValidator::new()))
            .with_cursor_config(CursorConfig {
                vi_insert: Some(SetCursorStyle::BlinkingBar),
                vi_normal: Some(SetCursorStyle::BlinkingBlock),
                emacs: Some(SetCursorStyle::DefaultUserShape),
            })
            .use_kitty_keyboard_enhancement(editor_config.use_kitty_keyboard_enhancement)
            .use_bracketed_paste(editor_config.use_bracketed_paste);

        if let Some(history_path) = editor_config.history_file_path {
            match FileBackedHistory::with_file(editor_config.history_max_size, history_path.clone())
            {
                Ok(history) => {
                    line_editor =
                        line_editor
                            .with_history(Box::new(history))
                            .with_hinter(Box::new(
                                DefaultHinter::default()
                                    .with_style(Style::new().fg(NuColor::DarkGray)),
                            ));
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to load history file '{}': {}",
                        history_path.display(),
                        e
                    );
                }
            }
        }

        let prompt = EditorPrompt::new();
        Editor {
            line_editor,
            prompt,
        }
    }

    pub fn get_input(&mut self) -> Input {
        let input_str = match self.line_editor.read_line(&self.prompt) {
            Ok(Signal::Success(buffer)) => buffer.trim().to_string(),
            Ok(Signal::CtrlD) | Ok(Signal::CtrlC) => {
                return Input::Command(Command::Exit);
            }
            x => {
                eprintln!("Error reading input: {:?}", x);
                return Input::Invalid;
            }
        };
        // print!("{}", "".resetting());
        match input_str {
            s if s.is_empty() => Input::Invalid,
            s => {
                if let Some(cmd) = parse_command(&s) {
                    Input::Command(cmd)
                } else {
                    Input::Message(s)
                }
            }
        }
    }
}

struct EditorPrompt {}

impl EditorPrompt {
    pub fn new() -> Self {
        EditorPrompt {}
    }
}

impl Prompt for EditorPrompt {
    fn render_prompt_left(&self) -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'static, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'static, str> {
        Cow::Borrowed("? ")
    }

    fn get_prompt_color(&self) -> Color {
        Color::Green
    }

    fn get_indicator_color(&self) -> Color {
        Color::Magenta
    }
}

struct PromptHighlighter {}

enum ParseState {
    Start,
    Message,
    CommandParsed,
    Invalid,
    // End,
}

impl PromptHighlighter {
    pub fn new() -> Self {
        PromptHighlighter {}
    }

    pub fn parse_line(&self, line: &str) -> Vec<(InputPart, String)> {
        let splits = line.split_inclusive(|c: char| c.is_whitespace());

        let mut parts = Vec::new();
        let mut parse_state: ParseState = ParseState::Start;
        // let mut available_arguments = 0;
        for split in splits {
            parse_state = match parse_state {
                ParseState::Start => {
                    if let Some(cmd) = parse_command(split) {
                        let parsed = split.split_inclusive('/').collect::<Vec<&str>>();
                        parts.push((InputPart::Slash, parsed[0].to_string()));
                        match cmd {
                            Command::Invalid => {
                                parts.push((InputPart::InvalidCommand, parsed[1..].join("")));
                                ParseState::Invalid
                            }
                            _ => {
                                parts.push((InputPart::ValidCommand(cmd), parsed[1..].join("")));
                                ParseState::CommandParsed
                            }
                        }
                    } else if split.chars().all(char::is_whitespace) {
                        parts.push((InputPart::Whitespace, split.to_string()));
                        ParseState::Start
                    } else {
                        parts.push((InputPart::MessageText, split.to_string()));
                        ParseState::Message
                    }
                }
                ParseState::Message => {
                    parts.push((InputPart::MessageText, split.to_string()));
                    ParseState::Message
                }
                ParseState::CommandParsed => {
                    // if available_arguments > 0 {
                    //     parts.push((InputPart::ValidArgument, split.to_string()));
                    //     ParseState::CommandParsed
                    // } else {
                    //     parts.push((InputPart::InvalidArgument, split.to_string()));
                    //     ParseState::Invalid
                    // }
                    parts.push((InputPart::ValidArgument, split.to_string()));
                    ParseState::CommandParsed
                }
                ParseState::Invalid => {
                    parts.push((InputPart::InvalidArgument, split.to_string()));
                    ParseState::Invalid
                }
            };
        }

        parts
    }
}

impl Highlighter for PromptHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let ranges = &self.parse_line(line);
        let mut styled_text = StyledText::new();
        for r in ranges {
            let style = r.0.style();
            let text = r.1.clone();
            styled_text.push((style, text));
        }
        styled_text
    }
}

#[derive(PartialEq)]
enum InputPart {
    Whitespace,
    Slash,
    MessageText,
    ValidCommand(Command),
    InvalidCommand,
    ValidArgument,
    InvalidArgument,
}

impl InputPart {
    fn style(&self) -> Style {
        match self {
            InputPart::Whitespace => Style::default(),
            InputPart::Slash => Style::default(),
            InputPart::MessageText => Style::new().fg(NuColor::Green),
            InputPart::ValidCommand(_) => Style::new().fg(NuColor::Cyan),
            InputPart::InvalidCommand => Style::new().fg(NuColor::Yellow),
            InputPart::ValidArgument => Style::new().fg(NuColor::Blue),
            InputPart::InvalidArgument => Style::new().on(NuColor::Red),
        }
    }
}

struct PromptValidator;

impl PromptValidator {
    pub fn new() -> Self {
        PromptValidator {}
    }
}

impl Validator for PromptValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.trim().is_empty() {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}
