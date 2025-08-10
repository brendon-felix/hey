/* -------------------------------------------------------------------------- */
/*                                 editor.rs                                  */
/* -------------------------------------------------------------------------- */

use std::borrow::Cow;

use reedline::{
    Reedline,
    Signal,
    Prompt,
    PromptEditMode,
    PromptHistorySearch,
    Color,
    Vi,
    CursorConfig,
    Highlighter,
    StyledText,
};

use nu_ansi_term::Style;
use nu_ansi_term::Color as NuColor;

use crossterm::cursor::SetCursorStyle;

use crate::commands::{Command, parse_command};

const PROMPT_INDICATOR: &str = env!("PROMPT_INDICATOR");
const PROMPT_MULTILINE_INDICATOR: &str = env!("PROMPT_MULTILINE_INDICATOR");

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
        let commands = Command::Exit.strings()
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

        let prompt = EditorPrompt {};
        Editor { line_editor, prompt }
    }

    pub fn get_input(&mut self) -> Input {
        let input_str = match self.line_editor.read_line(&self.prompt) {
            Ok(Signal::Success(buffer)) => {
                buffer.trim().to_string()
            }
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
            },
        }
    }
}

struct EditorPrompt {
}

impl Prompt for EditorPrompt {
    fn render_prompt_left(&self) -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'static, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'static, str> {
        Cow::Borrowed(PROMPT_INDICATOR)
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'static, str> {
        Cow::Borrowed(PROMPT_MULTILINE_INDICATOR)
    }

    fn render_prompt_history_search_indicator(&self, _history_search: PromptHistorySearch) -> Cow<'static, str> {
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

impl PromptHighlighter {
    pub fn new(commands: Vec<String>) -> Self {
        PromptHighlighter { commands }
    }
}

impl Highlighter for PromptHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let buffer = if line.starts_with('/') {
            let command = line[1..].split_whitespace().next().unwrap_or("");
            if self.commands.contains(&command.to_string()) {
                vec![
                    (Style::default(), '/'.to_string()),
                    (Style::new().fg(NuColor::Cyan), command.to_string()),
                    (Style::new().fg(NuColor::Green), line[command.len() + 1..].to_string()),
                ]
            } else {
                vec![
                    (Style::default(), '/'.to_string()),
                    (Style::new().fg(NuColor::Yellow), command.to_string()),
                    (Style::new().on(NuColor::Red), line[command.len() + 1..].to_string()),
                ]
            }
        } else {
            vec![(Style::new().fg(NuColor::Green), line.to_string())]
        };

        StyledText { buffer }
    }
}

