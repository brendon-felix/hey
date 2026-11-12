/* -------------------------------------------------------------------------- */
/*                               render/mod.rs                                */
/* -------------------------------------------------------------------------- */

mod highlight;

use std::io::{Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use ansi_parser::{AnsiParser, Output};
use rig_core::message::{AssistantContent, Message, ToolResult, UserContent};
use unicode_segmentation::UnicodeSegmentation;
use yansi::Paint;

use crate::config::Display;
use crate::mention;

pub use highlight::{Highlighter, themes};

static ANIMATIONS: AtomicBool = AtomicBool::new(false);

pub fn configure(display: &Display) {
    if display.colors {
        yansi::enable();
    } else {
        yansi::disable();
    }
    ANIMATIONS.store(display.animations, Ordering::Relaxed);
}

/// How replies are drawn; changed at runtime by `/theme`.
#[derive(Debug, Clone)]
pub struct Style {
    pub theme: Option<String>,
    pub wrap: Option<usize>,
}

impl From<&Display> for Style {
    fn from(display: &Display) -> Self {
        Self {
            theme: display
                .syntax_highlighting
                .then(|| display.theme.clone()),
            wrap: (display.wrap_width > 0).then_some(display.wrap_width),
        }
    }
}

impl Style {
    fn highlighter(&self) -> Option<Highlighter> {
        let theme = self.theme.as_deref()?;
        Highlighter::new(theme)
            .inspect_err(|error| notice(&format!("highlighting disabled: {error}")))
            .ok()
    }

    fn wrap(&self, line: &str) -> String {
        let Some(width) = self.wrap else {
            return line.to_owned();
        };
        textwrap::wrap(line, width.min(terminal_width())).join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Text,
    Reasoning,
}

/// A reply drawn as it streams in: complete lines are highlighted and
/// wrapped, and the open line waits for its end.
pub struct Reply {
    style: Style,
    animate: bool,
    part: Option<Part>,
    highlighter: Option<Highlighter>,
    pending: String,
}

impl Reply {
    pub fn new(style: &Style) -> Self {
        Self {
            style: style.clone(),
            animate: true,
            part: None,
            highlighter: None,
            pending: String::new(),
        }
    }

    pub fn instant(style: &Style) -> Self {
        Self {
            animate: false,
            ..Self::new(style)
        }
    }

    pub async fn text(&mut self, fragment: &str) {
        self.push(Part::Text, fragment).await;
    }

    pub async fn reasoning(&mut self, fragment: &str) {
        self.push(Part::Reasoning, fragment).await;
    }

    /// Ends the open part, so whatever prints next starts on its own line.
    pub async fn finish(&mut self) {
        if let Some(part) = self.part.take()
            && !self.pending.is_empty()
        {
            let mut line = std::mem::take(&mut self.pending);
            line.push('\n');
            self.print(part, &line).await;
        }
    }

    async fn push(&mut self, part: Part, fragment: &str) {
        if self.part != Some(part) {
            self.finish().await;
            self.part = Some(part);
            self.highlighter = (part == Part::Text)
                .then(|| self.style.highlighter())
                .flatten();
            println!();
        }
        self.pending.push_str(fragment);
        while let Some(end) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=end).collect();
            self.print(part, &line).await;
        }
    }

    async fn print(&mut self, part: Part, line: &str) {
        let painted = match (part, &mut self.highlighter) {
            (Part::Text, Some(highlighter)) => highlighter.line(line),
            (Part::Text, None) => line.to_owned(),
            (Part::Reasoning, _) => line.dim().italic().to_string(),
        };
        let wrapped = self.style.wrap(&painted);
        if self.animate {
            animate(&wrapped, 5000).await;
        } else {
            print!("{wrapped}");
        }
    }
}

pub fn user_message(text: &str) {
    println!("\n{}{}", "> ".magenta(), text.green());
}

pub fn tool_call(name: &str, args: &str) {
    let room = terminal_width().saturating_sub(name.chars().count() + 3);
    println!(
        "\n{} {} {}",
        "●".cyan(),
        name.bold(),
        clip(&summarize_args(args), room).dim()
    );
}

pub fn tool_result(result: &ToolResult) {
    let text = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    let room = terminal_width().saturating_sub(result.name.as_str().chars().count() + 6);
    if result.is_error {
        let first = text.lines().next().unwrap_or("failed");
        println!("  {} {} {}", "✗".red(), result.name.as_str().red(), clip(first, room).red());
        return;
    }
    let summary = match text.lines().count() {
        0 => "done".to_owned(),
        1 => text.trim().to_owned(),
        lines => format!("{lines} lines"),
    };
    println!("  {} {} {}", "↳".dim(), result.name.as_str().dim(), clip(&summary, room).dim());
}

/// One piece of a running command's output, as the shell tool reports it.
pub fn live_output(text: &str) {
    let line = text.strip_suffix('\n').unwrap_or(text);
    println!("  {} {}", "│".dim(), line.dim());
}

/// What a mention attached to the message being sent.
pub fn attached(target: &dyn std::fmt::Display) {
    println!("  {} {}", "@".cyan(), target.to_string().dim());
}

pub fn notice(text: &str) {
    println!("  {}", text.dim());
}

pub fn error(error: &dyn std::fmt::Display) {
    snailprint(&format!("\n{} {error}\n", "Error:".red()), 2000);
}

pub async fn conversation(messages: &[Message], style: &Style) {
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => content.iter().for_each(|content| match content {
                UserContent::Text(text) if !mention::is_context(&text.text) => {
                    user_message(&text.text)
                }
                UserContent::ToolResult(result) => tool_result(result),
                _ => {}
            }),
            Message::Assistant(assistant) => {
                let mut reply = Reply::instant(style);
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text(text) => reply.text(&text.text).await,
                        AssistantContent::ToolCall(call) => {
                            reply.finish().await;
                            let args = serde_json::Value::Object(call.function.arguments.clone());
                            tool_call(call.function.name.as_str(), &args.to_string());
                        }
                        _ => {}
                    }
                }
                reply.finish().await;
            }
        }
    }
    println!();
}

/// The argument a person would recognise a call by: a well-known one, else
/// the only string argument, else all of them.
fn summarize_args(args: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(args) else {
        return args.to_owned();
    };
    let field = |key| map.get(key).and_then(serde_json::Value::as_str);
    let mut strings = map.values().filter_map(serde_json::Value::as_str);
    let only_string = match (strings.next(), strings.next()) {
        (Some(only), None) if map.len() == 1 => Some(only),
        _ => None,
    };
    match (field("from"), field("to")) {
        (Some(from), Some(to)) => format!("{from} → {to}"),
        _ => ["command", "path", "pattern", "url", "query"]
            .into_iter()
            .find_map(field)
            .or(only_string)
            .map_or_else(|| serde_json::Value::Object(map.clone()).to_string(), str::to_owned),
    }
}

fn clip(text: &str, width: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= width && !text.contains('\n') {
        return line.to_owned();
    }
    let kept: String = line.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

pub fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(80, |(width, _)| usize::from(width))
}

/// Prints `text` a grapheme at a time when animations are on, leaving ANSI
/// escapes whole.
pub fn snailprint(text: &str, micros: u64) {
    if !ANIMATIONS.load(Ordering::Relaxed) {
        return show(text);
    }
    for frame in frames(text) {
        if let Output::TextBlock(_) = frame {
            sleep(Duration::from_micros(micros));
        }
        show(frame);
    }
}

/// [`snailprint`] that waits on the runtime's timer, so whoever awaits it can
/// stop it midway, as Ctrl+C does to a streaming reply.
async fn animate(text: &str, micros: u64) {
    if !ANIMATIONS.load(Ordering::Relaxed) {
        return show(text);
    }
    for frame in frames(text) {
        if let Output::TextBlock(_) = frame {
            tokio::time::sleep(Duration::from_micros(micros)).await;
        }
        show(frame);
    }
}

/// What an animation prints at each step: a grapheme, or an ANSI escape.
fn frames(text: &str) -> impl Iterator<Item = Output<'_>> {
    text.ansi_parse().flat_map(|output| match output {
        Output::TextBlock(block) => block.graphemes(true).map(Output::TextBlock).collect(),
        escape => vec![escape],
    })
}

fn show(text: impl std::fmt::Display) {
    print!("{text}");
    let _ = stdout().flush();
}
