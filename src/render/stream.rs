/* -------------------------------------------------------------------------- */
/*                              render/stream.rs                              */
/* -------------------------------------------------------------------------- */

use std::collections::VecDeque;
use std::fmt;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use crossterm::Command;
use crossterm::cursor::MoveUp;
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};

use super::preview::Call;
use super::reply::{Part, Reply};
use super::tool::{Mark, PREVIEW_LINES, line, row};
use super::{
    Style, animations, clip, clock, show, terminal_height, terminal_width, theme, thought,
};
use crate::config::ToolOutput;

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
    /// Hidden reasoning that took this long ended.
    Thought(Duration),
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

/// The rows under the reply that say what the agent is doing, as drawn.
struct Status {
    text: String,
    rows: usize,
}

/// How a running command's output shows, per [`ToolOutput`].
enum Output {
    Full,
    Preview(Preview),
    Hidden,
}

impl From<ToolOutput> for Output {
    fn from(setting: ToolOutput) -> Self {
        match setting {
            ToolOutput::Full => Output::Full,
            ToolOutput::Preview => Output::Preview(Preview::default()),
            ToolOutput::Hidden => Output::Hidden,
        }
    }
}

/// The latest lines of a running command, redrawn in place as more arrive,
/// and left in place once it ends. Output that is not a terminal gets them
/// once the command's output ends.
#[derive(Default)]
struct Preview {
    lines: VecDeque<String>,
    /// Rows the lines take on the terminal, right above the cursor.
    drawn: usize,
}

impl Preview {
    fn push(&mut self, text: String, mode: Mode) {
        if self.lines.len() == PREVIEW_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(text);
        if mode != Mode::Lines {
            self.draw();
        }
    }

    /// Leaves the lines shown and starts a new preview below them.
    fn end(&mut self, mode: Mode) {
        if mode == Mode::Lines {
            self.draw();
        }
        *self = Self::default();
    }

    fn draw(&mut self) {
        if let Ok(rows @ 1..) = u16::try_from(self.drawn) {
            show(format!("{}\r{}", MoveUp(rows), Clear(ClearType::FromCursorDown)));
        }
        let rows: String = self.lines.iter().map(|text| row(Mark::Printed, text) + "\n").collect();
        show(rows);
        self.drawn = self.lines.len();
    }
}

/// A run's reply on the terminal, advanced a frame at a time, with a status
/// while the agent works without saying anything.
pub struct Stream {
    mode: Mode,
    reply: Reply,
    show_thinking: bool,
    activity: Activity,
    output: Output,
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
        let now = Instant::now();
        Self {
            mode,
            reply: Reply::new(style),
            show_thinking: style.show_thinking,
            activity: Activity::Waiting,
            output: style.tool_output.into(),
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
        if self.show_thinking {
            self.receive(Part::Reasoning, text);
        } else {
            self.think();
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

    /// A part of reasoning ended after `duration`, which hidden reasoning
    /// leaves on screen in its place.
    pub fn thought(&mut self, duration: Duration) {
        self.end_part();
        if !self.show_thinking {
            self.backlog.push_back(Chunk::Thought(duration));
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

    pub fn running(&mut self, tool: &str) {
        self.activity = Activity::Running(tool.to_owned());
    }

    /// The model has the tools' results and has yet to answer them.
    pub fn waiting(&mut self) {
        self.activity = Activity::Waiting;
    }

    /// A line of a running command's output.
    pub fn output(&mut self, text: String) {
        if let Output::Hidden = self.output {
            return;
        }
        self.flush();
        match &mut self.output {
            Output::Full => println!("{}", line(Mark::Printed, &text)),
            Output::Preview(preview) => preview.push(text, self.mode),
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
        if self.reply.at_line_start() && self.quiet_since.elapsed() >= SPINNER_DELAY {
            self.draw_status();
        }
    }

    /// Shows everything received and ends the open part, so other output
    /// can follow.
    pub fn settle(&mut self) {
        self.end_preview();
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

    fn end_preview(&mut self) {
        if let Output::Preview(preview) = &mut self.output {
            preview.end(self.mode);
        }
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
            self.end_preview();
        }
        while let Some(chunk) = self.backlog.pop_front() {
            match chunk {
                Chunk::End => self.reply.finish(),
                Chunk::Thought(duration) => thought(Some(duration)),
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

    /// Draws the status: a call being written, if there is one, over the
    /// spinner. Each row is narrower than the terminal, so none wraps and
    /// the rows can be found again to redraw.
    fn draw_status(&mut self) {
        let elapsed = self.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize;
        let palette = theme::palette();
        let spinner = format!(
            "{} {}",
            palette.accent.paint(SPINNER[frame % SPINNER.len()]),
            palette.muted.paint(&clip(
                &format!("{} {}", self.activity, clock(elapsed)),
                terminal_width().saturating_sub(3)
            )),
        );
        let mut rows = match &self.activity {
            Activity::Preparing(call) => {
                call.rows(terminal_width().saturating_sub(1), terminal_height().saturating_sub(2))
            }
            _ => Vec::new(),
        };
        rows.push(spinner);
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
