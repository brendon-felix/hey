/* -------------------------------------------------------------------------- */
/*                              render/stream.rs                              */
/* -------------------------------------------------------------------------- */

use std::collections::VecDeque;
use std::fmt;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use rig_core::message::{CallId, ToolResult};
use rig_tools::split_status;
use yansi::Paint;

use super::preview::{Call, Preview, Source};
use super::reply::{Part, Reply};
use super::tool::{self, Mark, line};
use super::{
    CallStatus, Style, animations, call_line, call_path, clip, compacted, link, live_clock, machine_arg,
    palette, screen, summarize_args, terminal_height, terminal_width, theme, thought, thought_text,
};
use crate::config::Output;

/// How often a [`Stream`] wants [`Stream::tick`] called.
pub const FRAME: Duration = Duration::from_millis(16);
/// Frames an animated reply takes to show what has arrived: it eases in, yet
/// never trails the model by more than this.
const CATCH_UP: usize = 6;
/// Quiet before the spinner shows, so it does not flicker between lines.
const SPINNER_DELAY: Duration = Duration::from_millis(250);
const SPINNER_FRAME: Duration = Duration::from_millis(80);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Output is not a terminal: whole lines, nothing redrawn.
    Lines,
    /// Text shows as soon as it arrives.
    Live,
    /// Text eases in.
    Animated,
}

enum Chunk {
    Text(Part, String),
    End,
    /// Hidden reasoning ended, having taken this long when that is known.
    Thought(Option<Duration>),
}

/// What ends a preview, which its box shows.
enum Ending<'a> {
    /// Something else shows below it.
    Interrupted,
    /// The reasoning it shows took this long.
    Thought(Duration),
    /// The result of the call it belongs to, which came to this where the
    /// box does not show it.
    Answered(&'a str),
}

/// The calls of the current batch: kept in the live region while any of
/// them still runs, and printed in order as those at the front finish.
#[derive(Default)]
struct Calls {
    entries: VecDeque<CallEntry>,
}

/// A call in the batch, from when it starts until its line is printed.
struct CallEntry {
    id: Option<CallId>,
    tool: String,
    machine: Option<String>,
    argument: String,
    /// The file it names, if any.
    path: Option<String>,
    state: CallState,
}

enum CallState {
    /// Its line was already printed above its body or before a question.
    Printed,
    /// Waiting in the live region for its result.
    Running,
    /// Answered while an earlier call in the batch still ran; its line and
    /// any rows below it wait for that earlier call to finish.
    Finished { line: String, body: Vec<String> },
}

impl CallEntry {
    fn line(&self, status: CallStatus<'_>, width: usize) -> String {
        call_line(
            &self.tool,
            self.machine.as_deref(),
            &self.argument,
            status,
            width,
        )
    }
}

impl Calls {
    fn push(&mut self, entry: CallEntry) {
        self.entries.push_back(entry);
    }

    fn position(&self, id: &CallId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.id.as_ref() == Some(id))
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut CallEntry> {
        self.entries.get_mut(index)
    }

    /// Prints every finished or already-printed call at the front of the
    /// batch, stopping at the first call still running.
    fn drain_ready(&mut self) {
        while let Some(front) = self.entries.front() {
            match &front.state {
                CallState::Running => break,
                CallState::Printed => {
                    self.entries.pop_front();
                }
                CallState::Finished { .. } => {
                    if let Some(CallEntry {
                        state: CallState::Finished { line, body },
                        ..
                    }) = self.entries.pop_front()
                    {
                        screen::line(format_args!("\n{line}"));
                        body.into_iter().for_each(screen::line);
                    }
                }
            }
        }
    }

    /// Prints every remaining call in the batch so other output can follow.
    fn flush_all(&mut self, width: usize) {
        while let Some(entry) = self.entries.pop_front() {
            match entry.state {
                CallState::Printed => {}
                CallState::Running => {
                    let line = entry.line(CallStatus::Running, width);
                    screen::line(format_args!("\n{line}"));
                }
                CallState::Finished { line, body } => {
                    screen::line(format_args!("\n{line}"));
                    body.into_iter().for_each(screen::line);
                }
            }
        }
    }

    /// Prints the latest call's line if it is still waiting in the live
    /// region, so its live output or preview can show right under it.
    fn print_latest(&mut self, width: usize) {
        if matches!(
            self.entries.back(),
            Some(CallEntry {
                state: CallState::Running,
                ..
            })
        ) {
            for entry in &mut self.entries {
                match std::mem::replace(&mut entry.state, CallState::Printed) {
                    CallState::Printed => {}
                    CallState::Running => {
                        let line = entry.line(CallStatus::Running, width);
                        screen::line(format_args!("\n{line}"));
                    }
                    CallState::Finished { line, body } => {
                        screen::line(format_args!("\n{line}"));
                        body.into_iter().for_each(screen::line);
                    }
                }
            }
        }
    }

    /// The rows of the batch still in the live region, in at most `height`
    /// rows.
    fn live_rows(&self, width: usize, height: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for entry in &self.entries {
            match &entry.state {
                CallState::Printed => {}
                CallState::Running => {
                    rows.push(String::new());
                    rows.push(link::linkify(&entry.line(CallStatus::Running, width)));
                }
                CallState::Finished { line, body } => {
                    rows.push(String::new());
                    rows.push(link::linkify(line));
                    rows.extend(body.iter().map(|row| link::linkify(row)));
                }
            }
        }
        let skip = rows.len().saturating_sub(height);
        rows.into_iter().skip(skip).collect()
    }
}

/// What the spinner says the agent is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Activity {
    /// The model has yet to say more: it is reading the conversation, a
    /// tool's result in it, or is still loading.
    Waiting,
    /// Reasoning that is not shown.
    Thinking,
    /// The model is writing this call.
    /// The model is writing a call to this tool.
    Preparing(String),
    Running(String),
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Activity::Waiting => f.write_str("Waiting for model"),
            Activity::Thinking => f.write_str("Thinking"),
            Activity::Preparing(tool) => write!(f, "Preparing {tool}"),
            Activity::Running(tool) => write!(f, "Running {tool}"),
        }
    }
}

/// A run's reply on the terminal, advanced a frame at a time, with a status
/// while the agent works without saying anything.
pub struct Stream {
    mode: Mode,
    reply: Reply,
    style: Style,
    activity: Activity,
    /// The latest of what the agent does besides answering, while it goes
    /// on. Output that is not a terminal gets it once it ends.
    preview: Option<Preview>,
    calls: Calls,
    /// The calls the model has written, or is writing, that have yet to
    /// run, in order.
    writing: Vec<Call>,
    /// Whether the latest call's output showed as it ran.
    ran_live: bool,
    /// Received but not yet shown.
    backlog: VecDeque<Chunk>,
    started: Instant,
    quiet_since: Instant,
}

impl Stream {
    pub fn new(style: &Style) -> Self {
        let mode = match (std::io::stdout().is_terminal(), animations()) {
            (false, _) => Mode::Lines,
            (true, false) => Mode::Live,
            (true, true) => Mode::Animated,
        };
        Self::with_mode(style, mode)
    }

    /// A stream that shows a conversation already had, as it showed live.
    pub fn replaying(style: &Style) -> Self {
        Self::with_mode(style, Mode::Lines)
    }

    fn with_mode(style: &Style, mode: Mode) -> Self {
        let now = Instant::now();
        Self {
            mode,
            reply: Reply::new(style),
            style: style.clone(),
            activity: Activity::Waiting,
            preview: None,
            calls: Calls::default(),
            writing: Vec::new(),
            ran_live: false,
            backlog: VecDeque::new(),
            started: now,
            quiet_since: now,
        }
    }

    /// Times the spinner from when the run began, not from when this
    /// client started following it.
    pub fn began(&mut self, elapsed: Duration) {
        self.started = Instant::now().checked_sub(elapsed).unwrap_or(self.started);
    }

    pub fn text(&mut self, text: String) {
        self.activity = Activity::Waiting;
        self.receive(Part::Text, text);
    }

    pub fn reasoning(&mut self, text: String) {
        match self.style.thinking_output {
            Output::Full => self.receive(Part::Reasoning, text),
            Output::Preview if !text.is_empty() => {
                self.think();
                self.preview(Source::Reasoning, &text);
            }
            Output::Preview | Output::Hidden => self.think(),
        }
    }

    /// A part ended. A call stays in preparation until it runs, however
    /// its part and those around it end.
    pub fn end_part(&mut self) {
        if !matches!(self.activity, Activity::Preparing(_)) {
            self.activity = Activity::Waiting;
        }
        self.backlog.push_back(Chunk::End);
    }

    /// A part of reasoning ended after `duration`, when that is known,
    /// which shows where its reasoning does not, or only in preview.
    pub fn thought(&mut self, duration: Option<Duration>) {
        self.end_part();
        match (self.style.thinking_output, &self.preview, duration) {
            (Output::Full, _, _) => {}
            (_, Some(preview), Some(duration)) if preview.source() == Source::Reasoning => {
                self.end_preview(Ending::Thought(duration));
            }
            (_, Some(preview), None) if preview.source() == Source::Reasoning => {
                self.end_preview(Ending::Interrupted);
            }
            // Reasoning whose text the provider withholds still took time.
            (Output::Preview | Output::Hidden, _, _) => {
                self.backlog.push_back(Chunk::Thought(duration));
            }
        }
    }

    /// The model began writing a call to `tool`, which ends the text before
    /// it.
    pub fn preparing(&mut self, tool: String) {
        self.writing.push(Call::new(tool.clone()));
        self.activity = Activity::Preparing(tool);
        self.end_reply();
    }

    /// More of the arguments of the call the model is writing, as raw JSON.
    pub fn arguments(&mut self, json: &str) {
        if let Some(call) = self.writing.last_mut() {
            call.push(json);
        }
    }

    /// A call runs: its line, updated in place with its result once it
    /// finishes, and what it changes when it writes to a file. `machine` is
    /// the remote client it runs on, if not the host.
    pub fn call(&mut self, name: &str, args: &str, machine: Option<&str>, id: Option<CallId>) {
        let written = self.writing.iter().position(|call| call.is(name, args));
        // Calls run in the order the model wrote them, should its arguments
        // have changed on the way.
        let written = written.or_else(|| self.writing.iter().position(|call| call.name() == name));
        if let Some(at) = written {
            self.writing.remove(at);
        }
        self.end_preview(Ending::Interrupted);
        self.flush();
        self.activity = Activity::Running(name.to_owned());
        let summary = summarize_args(args);
        let machine = machine.map(str::to_owned).or_else(|| machine_arg(args));
        let path = call_path(args);
        self.ran_live = false;
        let width = terminal_width();
        let entry = CallEntry {
            id,
            tool: name.to_owned(),
            machine,
            argument: summary,
            path: path.clone(),
            state: CallState::Running,
        };
        // The table is for the user, so it shows whatever tool output does.
        if name == "table"
            && let Some(table) = crate::table::pretty(args)
        {
            self.calls.flush_all(width);
            screen::line(format_args!("\n{}", entry.line(CallStatus::Running, width)));
            table.lines().for_each(screen::line);
            self.calls.push(CallEntry {
                state: CallState::Printed,
                ..entry
            });
            return;
        }
        if let Some(todos) = tool::todos(name, args) {
            self.calls.flush_all(width);
            screen::line(format_args!("\n{}", entry.line(CallStatus::Running, width)));
            todos.iter().for_each(|todo| screen::line(todo));
            self.calls.push(CallEntry {
                state: CallState::Printed,
                ..entry
            });
            return;
        }
        let Some(changes) = tool::changes(name, args) else {
            self.calls.push(entry);
            return;
        };
        self.calls.flush_all(width);
        screen::line(format_args!("\n{}", entry.line(CallStatus::Running, width)));
        self.calls.push(CallEntry {
            state: CallState::Printed,
            ..entry
        });
        match self.style.tool_output {
            Output::Full => changes
                .iter()
                .for_each(|(mark, text)| screen::line(line(*mark, text))),
            Output::Preview => {
                let mut preview = Preview::new(Source::Changes);
                preview.code_of(self.style.highlight.then_some(path).flatten());
                preview.push(
                    &changes
                        .iter()
                        .map(|(mark, text)| format!("{}{text}\n", mark.sign()))
                        .collect::<String>(),
                );
                self.preview = Some(preview);
            }
            Output::Hidden => {}
        }
    }

    /// A call's result: updates the call's line in place with its status,
    /// or shows its output or error below a call whose line was already
    /// printed.
    pub fn result(&mut self, result: &ToolResult) {
        self.activity = Activity::Waiting;
        let text = tool::text(result);
        let summary = match result.is_error {
            true => text.lines().next().unwrap_or("failed").to_owned(),
            false => tool::summary(&text),
        };
        let index = self.calls.position(&result.call);
        let is_latest = index.is_some_and(|at| at + 1 == self.calls.entries.len());
        let own_box = match &self.preview {
            Some(preview) if !result.is_error && is_latest => Some(preview.source())
                .filter(|source| matches!(source, Source::Command | Source::Changes)),
            _ => None,
        };
        if let Some(source) = own_box {
            // A command's box shows what its result holds; what a file's
            // changes came to shows nowhere else.
            let outcome = match source {
                Source::Changes => summary.as_str(),
                _ => "",
            };
            self.end_preview(Ending::Answered(outcome));
            if let Some(at) = index
                && let Some(entry) = self.calls.get_mut(at)
            {
                entry.state = CallState::Printed;
            }
            self.calls.drain_ready();
            return self.flush();
        }
        self.end_preview(Ending::Interrupted);
        self.flush();
        let width = terminal_width();
        let name = result.name.as_str();
        let (source, given) = match name {
            "shell" | "bash" => (Source::Command, split_status(&text).0),
            "read" if self.style.highlight => (Source::Returned, super::read_lines(&text)),
            _ => (Source::Returned, text.as_str()),
        };
        let shown = self.ran_live || tool::shows_changes(name) || given.trim().is_empty();
        let path = index
            .and_then(|at| self.calls.entries.get(at))
            .and_then(|entry| entry.path.clone());
        let body = if result.is_error {
            Vec::new()
        } else {
            match self.style.tool_output {
                Output::Preview if !shown && given.lines().count() > 1 => {
                    let mut preview = Preview::new(source);
                    if name == "read" && self.style.highlight {
                        preview.code_of(path);
                    }
                    preview.push(given);
                    preview.framed(self.preview_width(), "")
                }
                Output::Full if !shown => given
                    .lines()
                    .map(|text| line(Mark::Printed, text))
                    .collect(),
                Output::Full | Output::Preview | Output::Hidden => Vec::new(),
            }
        };
        let status = match result.is_error {
            true => CallStatus::Failed(&summary),
            false => CallStatus::Done(&summary),
        };
        if let Some(at) = index
            && let Some(entry) = self.calls.get_mut(at)
        {
            match entry.state {
                CallState::Running => {
                    entry.state = CallState::Finished {
                        line: entry.line(status, width),
                        body,
                    };
                    self.calls.drain_ready();
                    return;
                }
                CallState::Printed | CallState::Finished { .. } => {
                    entry.state = CallState::Printed;
                    self.calls.drain_ready();
                }
            }
        }
        let room = width.saturating_sub(5);
        if result.is_error {
            screen::line(format_args!(
                "  {} {}",
                "✗".red(),
                clip(&summary, room).red()
            ));
            return;
        }
        body.into_iter().for_each(screen::line);
        let muted = palette().muted;
        screen::line(format_args!(
            "  {} {}",
            muted.paint("╰─"),
            muted.paint(&clip(&summary, room))
        ));
    }

    /// More of a running command's output: whole lines, but for its end.
    pub fn output(&mut self, text: String) {
        self.ran_live = true;
        self.calls.print_latest(terminal_width());
        match self.style.tool_output {
            Output::Full => {
                self.settle();
                text.split_inclusive('\n')
                    .for_each(|text| screen::line(line(Mark::Printed, text)));
            }
            Output::Preview => self.preview(Source::Command, &text),
            Output::Hidden => {}
        }
    }

    /// A step a subagent took, as [`super::agent_step`] puts it.
    pub fn agent_step(&mut self, step: String) {
        self.calls.print_latest(terminal_width());
        match self.style.subagent_output {
            Output::Full => {
                self.settle();
                let muted = palette().muted;
                screen::line(format_args!(
                    "  {}",
                    muted.paint(&clip(&step, terminal_width().saturating_sub(2)))
                ));
            }
            Output::Preview => self.preview(Source::Agents, &format!("{step}\n")),
            Output::Hidden => {}
        }
    }

    pub fn tick(&mut self) {
        if !self.backlog.is_empty() {
            self.reveal(self.budget());
            self.quiet_since = Instant::now();
        }
        if self.mode == Mode::Lines {
            return;
        }
        let tail = self.reply.tail();
        let rows = tail
            .iter()
            .cloned()
            .chain(self.status(tail.iter().len()))
            .collect();
        screen::set_live(rows);
    }

    /// Shows everything received and ends the open part and preview, so
    /// other output can follow.
    pub fn settle(&mut self) {
        self.calls.flush_all(terminal_width());
        self.end_preview(Ending::Interrupted);
        self.flush();
    }

    /// The conversation was compacted to `summary`, from and to about
    /// `sizes` tokens when known.
    pub fn compacted(&mut self, summary: &str, sizes: Option<(u64, u64)>) {
        self.settle();
        compacted(summary, sizes, &self.style);
    }

    /// [`Self::settle`], but a preview carries on: only text revealed
    /// below it ends it.
    fn flush(&mut self) {
        self.hide_status();
        self.reveal(usize::MAX);
        self.reply.finish();
        self.quiet_since = Instant::now();
    }

    /// Adds `text` to the preview of `source`, which begins below what has
    /// arrived before it.
    fn preview(&mut self, source: Source, text: &str) {
        if !self.backlog.is_empty() {
            self.flush();
        }
        if self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.source() != source)
        {
            self.end_preview(Ending::Interrupted);
        }
        self.preview
            .get_or_insert_with(|| Preview::new(source))
            .push(text);
    }

    /// Leaves the preview shown for good, with what ended it.
    fn end_preview(&mut self, ending: Ending) {
        let Some(mut preview) = self.preview.take() else {
            return;
        };
        self.hide_status();
        if preview.source() == Source::Reasoning {
            let took = match ending {
                Ending::Thought(duration) => Some(duration),
                Ending::Interrupted | Ending::Answered(_) => None,
            };
            preview.label(thought_text(took));
        }
        let bottom = match ending {
            Ending::Answered(marker) => marker,
            Ending::Interrupted | Ending::Thought(_) => "",
        };
        let rows = preview.framed(self.preview_width(), bottom);
        rows.into_iter().for_each(screen::line);
    }

    fn preview_width(&self) -> usize {
        self.style.width().min(terminal_width().saturating_sub(1))
    }

    fn receive(&mut self, part: Part, text: String) {
        match self.backlog.back_mut() {
            Some(Chunk::Text(last, queued)) if *last == part => queued.push_str(&text),
            _ => self.backlog.push_back(Chunk::Text(part, text)),
        }
    }

    /// Hidden reasoning ends the text before it, as shown reasoning would,
    /// so the status has a line of its own.
    fn think(&mut self) {
        if self.activity == Activity::Thinking {
            return;
        }
        self.activity = Activity::Thinking;
        self.end_reply();
    }

    fn end_reply(&mut self) {
        // With nothing queued, ending the reply directly keeps a status
        // already showing from blinking off.
        if self.backlog.is_empty() {
            self.reply.finish();
        } else {
            self.backlog.push_back(Chunk::End);
        }
    }

    /// How many characters this frame shows.
    fn budget(&self) -> usize {
        match self.mode {
            Mode::Lines | Mode::Live => usize::MAX,
            Mode::Animated => self
                .backlog
                .iter()
                .map(|chunk| match chunk {
                    Chunk::Text(_, text) => text.chars().count(),
                    Chunk::End | Chunk::Thought(_) => 0,
                })
                .sum::<usize>()
                .div_ceil(CATCH_UP),
        }
    }

    /// Hands the reply up to `budget` characters of the backlog, in order.
    fn reveal(&mut self, mut budget: usize) {
        if !self.backlog.is_empty() {
            self.calls.flush_all(terminal_width());
            self.end_preview(Ending::Interrupted);
        }
        while let Some(chunk) = self.backlog.pop_front() {
            match chunk {
                Chunk::End => self.reply.finish(),
                Chunk::Thought(duration) => thought(duration),
                Chunk::Text(part, mut text) => {
                    let end = text
                        .char_indices()
                        .nth(budget)
                        .map_or(text.len(), |(at, _)| at);
                    let rest = text.split_off(end);
                    if !text.is_empty() {
                        self.reply.push(part, &text);
                    }
                    budget -= text.chars().count();
                    if !rest.is_empty() {
                        self.backlog.push_front(Chunk::Text(part, rest));
                        return;
                    }
                }
            }
        }
    }

    /// The status: the calls of the current batch still waiting in the live
    /// region, the preview, if there is one, then the calls the model has
    /// written that have yet to run, then, once the agent has been quiet a
    /// while, the spinner, below `above` rows. Each row is narrower than
    /// the terminal, so none wraps.
    fn status(&self, above: usize) -> Vec<String> {
        let room = terminal_height().saturating_sub(above + 2);
        let mut rows = self.calls.live_rows(terminal_width(), room);
        if let Some(preview) = &self.preview {
            rows.extend(preview.framed(self.preview_width(), ""));
        }
        rows.extend(self.written(room.saturating_sub(rows.len())));
        if self.quiet_since.elapsed() >= SPINNER_DELAY {
            rows.push(self.spinner());
        }
        rows
    }

    /// The calls the model has written that have yet to run, in at most
    /// `height` rows: the latest first given room, as the one most likely
    /// still being written.
    fn written(&self, mut height: usize) -> Vec<String> {
        let mut calls: Vec<Vec<String>> = Vec::new();
        for call in self.writing.iter().rev() {
            let rows = call.rows(self.preview_width(), height);
            height -= rows.len();
            calls.push(rows);
        }
        calls.into_iter().rev().flatten().collect()
    }

    fn spinner(&self) -> String {
        let elapsed = self.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize;
        let palette = theme::palette();
        format!(
            "{} {}",
            palette.accent.paint(SPINNER[frame % SPINNER.len()]),
            palette.muted.paint(&clip(
                &format!("{} {}", self.activity, live_clock(elapsed)),
                terminal_width().saturating_sub(3)
            )),
        )
    }

    fn hide_status(&mut self) {
        if self.mode != Mode::Lines {
            screen::set_live(Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::{CallId, ToolName, ToolResult, ToolResultContent};

    use super::*;

    fn test_style(tool_output: Output) -> Style {
        Style {
            highlight: false,
            wrap: None,
            thinking_output: Output::Hidden,
            tool_output,
            subagent_output: Output::Hidden,
            notify_after: None,
        }
    }

    fn tool_result(id: &str, name: &str, text: &str, is_error: bool) -> ToolResult {
        ToolResult {
            call: CallId::from_wire(id),
            name: ToolName::new(name).unwrap(),
            content: vec![ToolResultContent::text(text)],
            is_error,
        }
    }

    #[test]
    fn batched_calls_update_in_place_and_drain_in_order() {
        yansi::disable();
        let mut stream = Stream::with_mode(&test_style(Output::Hidden), Mode::Live);
        stream.call(
            "read",
            r#"{"path":"/a.rs","machine":"turing"}"#,
            Some("turing"),
            Some(CallId::from_wire("1")),
        );
        stream.call(
            "read",
            r#"{"path":"/b.rs","machine":"fermi"}"#,
            Some("fermi"),
            Some(CallId::from_wire("2")),
        );
        assert_eq!(
            stream.calls.live_rows(80, 24),
            ["", "● read on turing /a.rs", "", "● read on fermi /b.rs",]
        );

        // Second call finishes first: its live row updates in place while the
        // first call is still running.
        stream.result(&tool_result("2", "read", "one\ntwo\nthree\n", false));
        assert_eq!(
            stream.calls.live_rows(80, 24),
            [
                "",
                "● read on turing /a.rs",
                "",
                "● read on fermi /b.rs · 3 lines",
            ]
        );

        // First call finishes: both drain from the live region in order.
        stream.result(&tool_result("1", "read", "alpha\nbeta\n", false));
        assert!(stream.calls.live_rows(80, 24).is_empty());
        assert!(stream.calls.entries.is_empty());
    }

    #[test]
    fn failed_call_updates_its_line_with_error_mark() {
        yansi::disable();
        let mut stream = Stream::with_mode(&test_style(Output::Hidden), Mode::Live);
        stream.call(
            "read",
            r#"{"path":"/first.rs"}"#,
            None,
            Some(CallId::from_wire("1")),
        );
        stream.call(
            "read",
            r#"{"path":"/missing.rs"}"#,
            None,
            Some(CallId::from_wire("2")),
        );
        stream.result(&tool_result("2", "read", "No such file or directory", true));
        assert_eq!(
            stream.calls.live_rows(80, 24),
            [
                "",
                "● read /first.rs",
                "",
                "✗ read /missing.rs · No such file or directory",
            ]
        );
    }
}
