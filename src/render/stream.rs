/* -------------------------------------------------------------------------- */
/*                              render/stream.rs                              */
/* -------------------------------------------------------------------------- */

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use crossterm::Command;
use crossterm::cursor::MoveUp;
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use rig_core::message::{CallId, ToolResult};
use rig_tools::split_status;
use yansi::Paint;

use super::preview::{Call, Preview, Source};
use super::reply::{Part, Reply};
use super::tool::{self, Mark, line};
use super::{
    Style, animations, call_line, clip, clock, machine_arg, palette, show, summarize_args,
    terminal_height, terminal_width, theme, thought, thought_text,
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
    /// The result of the call it belongs to, which its marker sums up.
    Answered(&'a str),
}

/// The calls shown whose results have yet to come.
#[derive(Default)]
struct Calls {
    /// Each call as its line names it.
    named: HashMap<CallId, String>,
    /// The call shown last, until a result comes.
    latest: Option<CallId>,
}

impl Calls {
    fn called(&mut self, id: Option<CallId>, named: String) {
        if let Some(id) = &id {
            self.named.insert(id.clone(), named);
        }
        self.latest = id;
    }

    /// Where `result` shows: right under its call, or apart from it, as
    /// when the calls of a reply run side by side.
    fn answered(&mut self, result: &ToolResult) -> Placement {
        let named = self.named.remove(&result.call);
        match self.latest.take() == Some(result.call.clone()) {
            true => Placement::Under,
            false => Placement::Apart(named),
        }
    }
}

enum Placement {
    Under,
    /// Apart from its call, which it names when known.
    Apart(Option<String>),
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
    Preparing(Call),
    Running(String),
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Activity::Waiting => f.write_str("Waiting for model"),
            Activity::Thinking => f.write_str("Thinking"),
            Activity::Preparing(call) => write!(f, "Preparing {}", call.name()),
            Activity::Running(tool) => write!(f, "Running {tool}"),
        }
    }
}

/// The rows at the bottom that are redrawn as they change, as drawn: a
/// preview, and what the agent is doing while it says nothing.
struct Status {
    text: String,
    rows: usize,
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
    /// Whether the latest call's output showed as it ran.
    ran_live: bool,
    /// Received but not yet shown.
    backlog: VecDeque<Chunk>,
    started: Instant,
    quiet_since: Instant,
    /// The status, while it shows.
    status: Option<Status>,
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
            ran_live: false,
            backlog: VecDeque::new(),
            started: now,
            quiet_since: now,
            status: None,
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
        self.activity = Activity::Preparing(Call::new(tool));
        self.end_reply();
    }

    /// More of the arguments of the call the model is writing, as raw JSON.
    pub fn arguments(&mut self, json: &str) {
        if let Activity::Preparing(call) = &mut self.activity {
            call.push(json);
        }
    }

    /// A call runs: its line, under which its output and result show, and
    /// what it changes when it writes to a file. `machine` is the remote
    /// client it runs on, if not the host.
    pub fn call(&mut self, name: &str, args: &str, machine: Option<&str>, id: Option<CallId>) {
        self.settle();
        self.activity = Activity::Running(name.to_owned());
        let summary = summarize_args(args);
        let machine = machine.map(str::to_owned).or_else(|| machine_arg(args));
        println!("\n{}", call_line(name, machine.as_deref(), &summary, terminal_width()));
        self.calls.called(id, format!("{name} {summary}"));
        self.ran_live = false;
        let Some(changes) = tool::changes(name, args) else {
            return;
        };
        match self.style.tool_output {
            Output::Full => changes.iter().for_each(|(mark, text)| println!("{}", line(*mark, text))),
            Output::Preview => {
                let mut preview = Preview::new(Source::Changes);
                preview.push(&changes.iter().map(|(mark, text)| format!("{}{text}\n", mark.sign())).collect::<String>());
                self.preview = Some(preview);
            }
            Output::Hidden => {}
        }
    }

    /// A call's result: what it gave back, where that has not shown as it
    /// ran or with the call, and its marker, which closes the call's box
    /// when it is still the last thing shown. A result shown apart from its
    /// call is set apart as a call is, and names it.
    pub fn result(&mut self, result: &ToolResult) {
        self.activity = Activity::Waiting;
        let text = tool::text(result);
        let placement = self.calls.answered(result);
        let summary = match result.is_error {
            true => text.lines().next().unwrap_or("failed").to_owned(),
            false => tool::summary(&text),
        };
        let own_box = !result.is_error
            && matches!(placement, Placement::Under)
            && self.preview.as_ref().is_some_and(|preview| matches!(preview.source(), Source::Command | Source::Changes));
        if own_box {
            self.end_preview(Ending::Answered(&summary));
            return self.flush();
        }
        self.settle();
        let called = match placement {
            Placement::Apart(Some(called)) => {
                println!();
                Some(called)
            }
            Placement::Under | Placement::Apart(None) => None,
        };
        let marker = |summary: &str| match &called {
            Some(called) => format!("{called} · {summary}"),
            None => summary.to_owned(),
        };
        let room = terminal_width().saturating_sub(5);
        if result.is_error {
            println!("  {} {}", "✗".red(), clip(&marker(&summary), room).red());
            return;
        }
        let name = result.name.as_str();
        let (source, given) = match name {
            "shell" => (Source::Command, split_status(&text).0),
            _ => (Source::Returned, text.as_str()),
        };
        let shown = self.ran_live || tool::shows_changes(name) || given.trim().is_empty();
        match self.style.tool_output {
            Output::Preview if !shown && given.lines().count() > 1 => {
                let mut preview = Preview::new(source);
                preview.label(called.clone().unwrap_or_default());
                preview.push(given);
                self.preview = Some(preview);
                return self.end_preview(Ending::Answered(&summary));
            }
            Output::Full if !shown => {
                given.lines().for_each(|text| println!("{}", line(Mark::Printed, text)));
            }
            Output::Full | Output::Preview | Output::Hidden => {}
        }
        let muted = palette().muted;
        println!("  {} {}", muted.paint("╰─"), muted.paint(&clip(&marker(&summary), room)));
    }

    /// A line of a running command's output.
    pub fn output(&mut self, text: String) {
        self.ran_live = true;
        match self.style.tool_output {
            Output::Full => {
                self.settle();
                println!("{}", line(Mark::Printed, &text));
            }
            Output::Preview => self.preview(Source::Command, &text),
            Output::Hidden => {}
        }
    }

    /// A step a subagent took, as [`super::agent_step`] puts it.
    pub fn agent_step(&mut self, step: String) {
        match self.style.subagent_output {
            Output::Full => {
                self.settle();
                let muted = palette().muted;
                println!("  {}", muted.paint(&clip(&step, terminal_width().saturating_sub(2))));
            }
            Output::Preview => self.preview(Source::Agents, &format!("{step}\n")),
            Output::Hidden => {}
        }
    }

    pub fn tick(&mut self) {
        if !self.backlog.is_empty() {
            self.hide_status();
            self.reveal(self.budget());
            self.quiet_since = Instant::now();
        }
        if self.mode == Mode::Lines {
            return;
        }
        self.reply.draw();
        if self.reply.at_line_start() {
            self.draw_status();
        }
    }

    /// Shows everything received and ends the open part and preview, so
    /// other output can follow.
    pub fn settle(&mut self) {
        self.end_preview(Ending::Interrupted);
        self.flush();
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
        if self.preview.as_ref().is_some_and(|preview| preview.source() != source) {
            self.end_preview(Ending::Interrupted);
        }
        self.preview.get_or_insert_with(|| Preview::new(source)).push(text);
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
        show(rows.iter().map(|row| format!("{row}\n")).collect::<String>());
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
            self.end_preview(Ending::Interrupted);
        }
        while let Some(chunk) = self.backlog.pop_front() {
            match chunk {
                Chunk::End => self.reply.finish(),
                Chunk::Thought(duration) => thought(duration),
                Chunk::Text(part, mut text) => {
                    let end = text.char_indices().nth(budget).map_or(text.len(), |(at, _)| at);
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

    /// Draws the status: the preview, if there is one, then, once the agent
    /// has been quiet a while, a call being written, if there is one, over
    /// the spinner. Each row is narrower than the terminal, so none wraps
    /// and the rows can be found again to redraw.
    fn draw_status(&mut self) {
        let mut rows = self
            .preview
            .as_ref()
            .map_or_else(Vec::new, |preview| preview.framed(self.preview_width(), ""));
        if self.quiet_since.elapsed() >= SPINNER_DELAY {
            let room = terminal_height().saturating_sub(rows.len() + 2);
            if let Activity::Preparing(call) = &self.activity {
                rows.extend(call.rows(self.preview_width(), room));
            }
            rows.push(self.spinner());
        }
        if rows.is_empty() {
            return self.hide_status();
        }
        let text = rows.join("\n");
        if self.status.as_ref().is_some_and(|status| status.text == text) {
            return;
        }
        show(format!(
            "{}{}{text}{}",
            ansi(BeginSynchronizedUpdate),
            self.erase_status(),
            ansi(EndSynchronizedUpdate)
        ));
        self.status = Some(Status {
            text,
            rows: rows.len(),
        });
    }

    fn spinner(&self) -> String {
        let elapsed = self.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize;
        let palette = theme::palette();
        format!(
            "{} {}",
            palette.accent.paint(SPINNER[frame % SPINNER.len()]),
            palette.muted.paint(&clip(
                &format!("{} {}", self.activity, clock(elapsed)),
                terminal_width().saturating_sub(3)
            )),
        )
    }

    fn hide_status(&mut self) {
        let erase = self.erase_status();
        if self.status.take().is_some() {
            show(erase);
        }
    }

    /// What returns the cursor to where the status began, clearing it.
    fn erase_status(&self) -> String {
        let up = match self.status.as_ref().map_or(0, |status| status.rows - 1) {
            0 => String::new(),
            rows => MoveUp(rows as u16).to_string(),
        };
        format!("\r{up}{}", Clear(ClearType::FromCursorDown))
    }
}

fn ansi(command: impl Command) -> String {
    let mut out = String::new();
    let _ = command.write_ansi(&mut out);
    out
}
