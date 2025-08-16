/* -------------------------------------------------------------------------- */
/*                                 editor.rs                                  */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;

use crate::commands::{Command, parse_command};
use crossterm::cursor::SetCursorStyle;
use nu_ansi_term::{Color as NuColor, Style};
use reedline::{
    Color, CursorConfig, Highlighter, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    Signal, StyledText, Vi,
};

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
        let commands = Command::Exit
            .strings()
            .into_iter()
            .chain(Command::Clear.strings())
            .chain(Command::Help.strings())
            .map(|s| s.to_string())
            .collect::<Vec<String>>();

        let line_editor = Reedline::create()
            .with_edit_mode(Box::new(Vi::default()))
            .with_highlighter(Box::new(PromptHighlighter::new(commands)))
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
    commands: Vec<String>,
}

enum CommandInputState {
    Slash,
    Recognized,
    Unrecognized,
    Argument,
    Invalid,
}

enum InputState {
    None,
    Message,
    Command(CommandInputState),
}

struct HighlightState {
    input_state: InputState,
    curr_style: Style,
}

impl HighlightState {
    fn new() -> Self {
        HighlightState {
            input_state: InputState::None,
            curr_style: Style::default(),
        }
    }

    pub fn next_char(&mut self, c: char, commands: &Vec<String>) -> (Style, String) {
        self.input_state = match &self.input_state {
            InputState::None => {
                if c == '/' {
                    InputState::Command(CommandInputState::Slash)
                } else if c.is_whitespace() {
                    InputState::None
                } else {
                    self.curr_style = Style::new().fg(NuColor::Green);
                    InputState::Message
                }
            }
            InputState::Message => InputState::Message,
            InputState::Command(CommandInputState::Slash) => {
                if c.is_whitespace() {
                    InputState::Command(CommandInputState::Slash)
                } else if commands.iter().any(|cmd| cmd.starts_with(c)) {
                    self.curr_style = Style::new().fg(NuColor::Cyan);
                    InputState::Command(CommandInputState::Recognized)
                } else {
                    self.curr_style = Style::new().fg(NuColor::Yellow);
                    InputState::Command(CommandInputState::Unrecognized)
                }
            }
            InputState::Command(CommandInputState::Recognized) => {
                if c.is_whitespace() {
                    self.curr_style = Style::new().fg(NuColor::Green);
                    InputState::Command(CommandInputState::Argument)
                } else {
                    InputState::Command(CommandInputState::Recognized)
                }
            }
            InputState::Command(CommandInputState::Unrecognized) => {
                if c.is_whitespace() {
                    InputState::Command(CommandInputState::Invalid)
                } else {
                    InputState::Command(CommandInputState::Unrecognized)
                }
            }
            InputState::Command(CommandInputState::Argument) => {
                if c.is_whitespace() {
                    InputState::Command(CommandInputState::Argument)
                } else {
                    InputState::Command(CommandInputState::Argument)
                }
            }
            InputState::Command(CommandInputState::Invalid) => {
                if c.is_whitespace() {
                    self.curr_style = Style::default();
                    InputState::Command(CommandInputState::Invalid)
                } else {
                    self.curr_style = Style::new().on(NuColor::Red);
                    InputState::Command(CommandInputState::Invalid)
                }
            }
        };
        (self.curr_style.clone(), c.to_string())
    }
}

impl PromptHighlighter {
    pub fn new(commands: Vec<String>) -> Self {
        PromptHighlighter { commands }
    }
}

impl Highlighter for PromptHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut state = HighlightState::new();
        let mut styled_text = StyledText::new();
        for c in line.chars() {
            styled_text.push(state.next_char(c, &self.commands));
        }
        styled_text
    }
}
