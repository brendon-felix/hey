/* -------------------------------------------------------------------------- */
/*                               render/mod.rs                                */
/* -------------------------------------------------------------------------- */

mod highlight;
mod preview;
mod reply;
mod stream;
mod theme;
mod wrap;

use std::io::{Write, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use rig_core::message::{AssistantContent, Message, ToolResult, UserContent};
use unicode_segmentation::UnicodeSegmentation;
use yansi::Paint;

use crate::config::Display;
use crate::mention;

pub use reply::{Reply, rows as reply_rows};
pub use stream::{FRAME, Stream};
pub use theme::{Color, Palette, Theme, palette, themes};

static ANIMATIONS: AtomicBool = AtomicBool::new(false);

pub fn configure(display: &Display) {
    if display.colors {
        yansi::enable();
    } else {
        yansi::disable();
    }
    ANIMATIONS.store(display.animations, Ordering::Relaxed);
    match Theme::load(&display.theme) {
        Ok(theme) => theme::set(theme),
        Err(error) => notice(&format!("{error:#}; using the terminal's colours")),
    }
}

fn animations() -> bool {
    ANIMATIONS.load(Ordering::Relaxed)
}

/// Switches every colour hey draws, from replies to the prompt.
pub fn set_theme(theme: Theme) {
    theme::set(theme);
}

pub fn current_theme() -> Arc<Theme> {
    theme::current()
}

/// How replies are drawn; the colours come from the current [`Theme`].
#[derive(Debug, Clone)]
pub struct Style {
    pub highlight: bool,
    /// Most columns a reply takes, when narrower than the terminal.
    pub wrap: Option<usize>,
    pub show_thinking: bool,
}

impl From<&Display> for Style {
    fn from(display: &Display) -> Self {
        Self {
            highlight: display.syntax_highlighting,
            wrap: (display.wrap_width > 0).then_some(display.wrap_width),
            show_thinking: display.show_thinking,
        }
    }
}

impl Style {
    fn width(&self) -> usize {
        let terminal = terminal_width();
        self.wrap.map_or(terminal, |wrap| wrap.min(terminal))
    }
}

pub fn user_message(text: &str) {
    println!("\n{}", user_line(text, palette()));
}

pub fn user_line(text: &str, palette: Palette) -> String {
    format!("{}{}", palette.accent.paint("> "), palette.input.paint(text))
}

pub fn tool_call(name: &str, args: &str) {
    println!("\n{}", call_line(name, &summarize_args(args), terminal_width()));
}

/// A tool call as it shows, `width` columns at most: its name and the
/// argument that identifies it.
fn call_line(name: &str, summary: &str, width: usize) -> String {
    let palette = palette();
    let room = width.saturating_sub(name.chars().count() + 3);
    format!(
        "{} {} {}",
        palette.name.paint("●"),
        name.bold(),
        palette.muted.paint(&clip(&summary.replace('\t', " "), room))
    )
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
    let muted = palette().muted;
    println!(
        "  {} {} {}",
        muted.paint("↳"),
        muted.paint(result.name.as_str()),
        muted.paint(&clip(&summary, room))
    );
}

/// One piece of a running command's output, as the shell tool reports it.
pub fn live_output(text: &str) {
    let line = text.strip_suffix('\n').unwrap_or(text);
    let muted = palette().muted;
    println!("  {} {}", muted.paint("│"), muted.paint(line));
}

/// What a mention attached to the message being sent.
pub fn attached(target: &dyn std::fmt::Display) {
    let palette = palette();
    println!("  {} {}", palette.name.paint("@"), palette.muted.paint(&target.to_string()));
}

/// That the model reasoned, where its reasoning is not shown, and for how
/// long when that is known.
pub fn thought(duration: Option<Duration>) {
    let text = match duration {
        Some(duration) => format!("Thought for {}", clock(duration)),
        None => "Thought".to_owned(),
    };
    println!("\n{}", palette().muted.paint(&text).italic());
}

fn clock(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds => format!("{}m {:02}s", seconds / 60, seconds % 60),
    }
}

pub fn notice(text: &str) {
    println!("  {}", palette().muted.paint(text));
}

pub fn error(error: &dyn std::fmt::Display) {
    println!("\n{} {error}", "Error:".red());
}

pub fn conversation(messages: &[Message], style: &Style) {
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
                let mut reply = Reply::new(style);
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text(text) => reply.text(&text.text),
                        AssistantContent::ToolCall(call) => {
                            reply.finish();
                            let args = serde_json::Value::Object(call.function.arguments.clone());
                            tool_call(call.function.name.as_str(), &args.to_string());
                        }
                        AssistantContent::Reasoning(_) => {
                            reply.finish();
                            thought(None);
                        }
                        _ => {}
                    }
                }
                reply.finish();
            }
        }
    }
    println!();
}

fn summarize_args(args: &str) -> String {
    match serde_json::from_str(args) {
        Ok(serde_json::Value::Object(map)) => summarize(&map),
        _ => args.to_owned(),
    }
}

/// The argument a person would recognise a call by: a well-known one, else
/// the only string argument, else all of them.
fn summarize(map: &serde_json::Map<String, serde_json::Value>) -> String {
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

pub fn clip(text: &str, width: usize) -> String {
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

fn terminal_height() -> usize {
    crossterm::terminal::size().map_or(24, |(_, height)| usize::from(height))
}

/// Prints `text` a grapheme at a time when animations are on.
pub fn snailprint(text: &str, micros: u64) {
    if !animations() {
        return show(text);
    }
    for grapheme in text.graphemes(true) {
        sleep(Duration::from_micros(micros));
        show(grapheme);
    }
}

fn show(text: impl std::fmt::Display) {
    print!("{text}");
    let _ = stdout().flush();
}
