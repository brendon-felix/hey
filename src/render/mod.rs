/* -------------------------------------------------------------------------- */
/*                               render/mod.rs                                */
/* -------------------------------------------------------------------------- */

mod highlight;
mod preview;
mod reply;
pub mod screen;
mod stream;
mod theme;
mod tool;
mod wrap;

use std::io::{Write, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use rig_core::message::{AssistantContent, Message, UserContent};
use textwrap::core::display_width;
use unicode_segmentation::UnicodeSegmentation;
use yansi::Paint;

use crate::config::{Display, Output};
use crate::mention;
use crate::protocol::Event;
use crate::route;
use crate::steer;

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
    pub thinking_output: Output,
    pub tool_output: Output,
    pub subagent_output: Output,
}

impl From<&Display> for Style {
    fn from(display: &Display) -> Self {
        Self {
            highlight: display.syntax_highlighting,
            wrap: (display.wrap_width > 0).then_some(display.wrap_width),
            thinking_output: display.thinking_output,
            tool_output: display.tool_output,
            subagent_output: display.subagent_output,
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
    screen::line(format_args!("\n{}", user_line(text, palette())));
}

pub fn user_line(text: &str, palette: Palette) -> String {
    format!("{}{}", palette.accent.paint("> "), palette.input.paint(text))
}

/// A tool call as it shows, `width` columns at most: its name, the machine
/// it runs on when that is named, and the argument that identifies it.
fn call_line(name: &str, machine: Option<&str>, summary: &str, width: usize) -> String {
    let palette = palette();
    let on = machine.map(|machine| format!(" on {machine}")).unwrap_or_default();
    let room = width.saturating_sub(name.chars().count() + on.chars().count() + 3);
    format!(
        "{} {}{} {}",
        palette.name.paint("●"),
        name.bold(),
        palette.muted.paint(&on),
        palette.muted.paint(&clip(&summary.replace('\t', " "), room))
    )
}

/// The machine a call's `machine` argument names.
fn machine_arg(args: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args).ok()?;
    args.get(route::ARG)?.as_str().map(str::to_owned)
}

/// A subagent began its task, `label`.
pub fn agent_started(label: &str) {
    screen::line(format_args!("\n{} {}", palette().name.paint("◇"), label.bold()));
}

/// What the subagent `label` did, as a line: a tool call or a notice.
pub fn agent_step(label: &str, event: &Event) -> Option<String> {
    let step = match event {
        Event::ToolCall { name, args, machine, .. } => {
            let on = machine.clone().or_else(|| machine_arg(args));
            let on = on.map(|machine| format!(" on {machine}")).unwrap_or_default();
            format!("{name}{on} {}", summarize_args(args))
        }
        Event::Notice { text } => text.clone(),
        _ => return None,
    };
    Some(format!("{label} › {}", step.replace('\n', " ")))
}

pub fn agent_ended(label: &str, error: Option<&str>) {
    let palette = palette();
    let room = terminal_width().saturating_sub(label.chars().count() + 6);
    match error {
        None => screen::line(format_args!("  {} {} {}", palette.name.paint("◆"), label, palette.muted.paint("done"))),
        Some(error) => screen::line(format_args!("  {} {} {}", "✗".red(), label.red(), clip(error, room).red())),
    }
}

/// What a mention attached to the message being sent.
pub fn attached(target: &dyn std::fmt::Display) {
    let palette = palette();
    screen::line(format_args!("  {} {}", palette.name.paint("@"), palette.muted.paint(&target.to_string())));
}

/// That the model reasoned, where its reasoning is not shown, and for how
/// long when that is known.
pub fn thought(duration: Option<Duration>) {
    screen::line(format_args!("\n{}", palette().muted.paint(&thought_text(duration)).italic()));
}

fn thought_text(duration: Option<Duration>) -> String {
    match duration {
        Some(duration) => format!("Thought for {}", clock(duration)),
        None => "Thought".to_owned(),
    }
}

fn clock(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds => format!("{}m {:02}s", seconds / 60, seconds % 60),
    }
}

pub fn notice(text: &str) {
    screen::line(format_args!("  {}", palette().muted.paint(text)));
}

pub fn error(error: &dyn std::fmt::Display) {
    screen::line(format_args!("\n{} {error}", "Error:".red()));
}

/// A conversation already had, shown as it showed live.
pub fn conversation(messages: &[Message], style: &Style) {
    let mut stream = Stream::replaying(style);
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => content.iter().for_each(|content| match content {
                UserContent::Text(text) if !mention::is_context(&text.text) => {
                    stream.settle();
                    user_message(&text.text)
                }
                UserContent::ToolResult(result) => {
                    let (result, steered) = steer::split(result.clone());
                    stream.result(&result);
                    for prompt in steered {
                        stream.settle();
                        user_message(&prompt);
                    }
                }
                _ => {}
            }),
            Message::Assistant(assistant) => {
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text(text) => stream.text(text.text.clone()),
                        AssistantContent::ToolCall(call) => {
                            let args = serde_json::Value::Object(call.function.arguments.clone());
                            stream.call(call.function.name.as_str(), &args.to_string(), None, Some(call.id.clone()));
                        }
                        AssistantContent::Reasoning(reasoning) => {
                            stream.reasoning(reasoning.text.clone());
                            stream.thought(None);
                        }
                        _ => {}
                    }
                }
                stream.end_part();
            }
        }
    }
    stream.settle();
    screen::line("");
}

fn summarize_args(args: &str) -> String {
    match tool_args(args) {
        Some(map) if map.is_empty() => String::new(),
        Some(map) => summarize(&map),
        None => args.to_owned(),
    }
}

/// A call's arguments as its tool takes them: without the machine it runs
/// on, which tools that refuse unknown arguments would reject.
fn tool_args(args: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str(args).ok()? {
        serde_json::Value::Object(mut map) => {
            map.remove(route::ARG);
            Some(map)
        }
        _ => None,
    }
}

/// The argument a person would recognise a call by: a well-known one, else
/// the only string argument, else all of them.
fn summarize(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let field = |key| map.get(key).and_then(serde_json::Value::as_str);
    if let Some(serde_json::Value::Array(agents)) = map.get("agents") {
        let labels: Vec<&str> = agents
            .iter()
            .filter_map(|agent| agent.get("label")?.as_str())
            .collect();
        return labels.join(", ");
    }
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

/// The first line of `text`, cut to `width` columns.
pub fn clip(text: &str, width: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if display_width(line) <= width && !text.contains('\n') {
        return line.to_owned();
    }
    let mut used = 0;
    let kept: String = line
        .chars()
        .take_while(|c| {
            used += display_width(c.encode_utf8(&mut [0; 4]));
            used < width
        })
        .collect();
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
