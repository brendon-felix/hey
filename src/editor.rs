/* -------------------------------------------------------------------------- */
/*                                 editor.rs                                  */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;

use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color as NuColor, Style};
use reedline::{
    Color, CursorConfig, Highlighter, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    Signal, StyledText, ValidationResult, Validator, Vi,
};

use crate::commands::{Command, parse_command};

pub enum Input {
    Command(Command),
    Message(String),
    Invalid,
}

pub struct Editor {
    line_editor: Reedline,
    prompt: EditorPrompt,
}

impl Editor {
    pub fn new() -> Self {
        let line_editor = Reedline::create()
            .with_edit_mode(Box::new(Vi::default()))
            .with_highlighter(Box::new(PromptHighlighter::new()))
            .with_validator(Box::new(PromptValidator::new()))
            .with_cursor_config(CursorConfig {
                vi_insert: Some(SetCursorStyle::BlinkingBar),
                vi_normal: Some(SetCursorStyle::BlinkingBlock),
                emacs: Some(SetCursorStyle::DefaultUserShape),
            })
            .use_kitty_keyboard_enhancement(true)
            .use_bracketed_paste(true);

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
        // let prompt_indicator =
        //     std::env::var("PROMPT_INDICATOR").unwrap_or_else(|_| "> ".to_string());
        // let prompt_multiline_indicator =
        //     std::env::var("PROMPT_MULTILINE_INDICATOR").unwrap_or_else(|_| "::: ".to_string());
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

struct PromptHighlighter {
    // commands: Vec<String>,
}

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
