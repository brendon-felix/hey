/* -------------------------------------------------------------------------- */
/*                              render/stream.rs                              */
/* -------------------------------------------------------------------------- */

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use crossterm::terminal::{Clear, ClearType};

use super::reply::{Part, Reply};
use super::{Style, animations, show, theme};

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
}

/// What the spinner says the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Working,
    /// Reasoning that is not shown.
    Thinking,
}

impl Activity {
    fn label(self) -> &'static str {
        match self {
            Activity::Working => "Working",
            Activity::Thinking => "Thinking",
        }
    }
}

/// A run's reply on the terminal, advanced a frame at a time, with a spinner
/// while the agent works without saying anything.
pub struct Stream {
    mode: Mode,
    reply: Reply,
    show_thinking: bool,
    activity: Activity,
    /// Received but not yet shown.
    backlog: VecDeque<Chunk>,
    started: Instant,
    quiet_since: Instant,
    /// The spinner row, while it shows.
    spinner: Option<String>,
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
            activity: Activity::Working,
            backlog: VecDeque::new(),
            started: now,
            quiet_since: now,
            spinner: None,
        }
    }

    /// Times the spinner from when the run began, not from when this
    /// client started following it.
    pub fn began(&mut self, elapsed: Duration) {
        self.started = Instant::now().checked_sub(elapsed).unwrap_or(self.started);
    }

    pub fn text(&mut self, text: String) {
        self.activity = Activity::Working;
        self.receive(Part::Text, text);
    }

    pub fn reasoning(&mut self, text: String) {
        if self.show_thinking {
            self.receive(Part::Reasoning, text);
        } else {
            self.think();
        }
    }

    pub fn end_part(&mut self) {
        self.activity = Activity::Working;
        self.backlog.push_back(Chunk::End);
    }

    pub fn tick(&mut self) {
        if !self.backlog.is_empty() {
            self.hide_spinner();
            self.reveal(self.budget());
            self.quiet_since = Instant::now();
        }
        if self.mode == Mode::Lines {
            return;
        }
        self.reply.draw();
        if self.reply.at_line_start() && self.quiet_since.elapsed() >= SPINNER_DELAY {
            self.spin();
        }
    }

    /// Shows everything received and ends the open part, so other output
    /// can follow.
    pub fn settle(&mut self) {
        self.activity = Activity::Working;
        self.hide_spinner();
        self.reveal(usize::MAX);
        self.reply.finish();
        self.quiet_since = Instant::now();
    }

    fn receive(&mut self, part: Part, text: String) {
        match self.backlog.back_mut() {
            Some(Chunk::Text(last, queued)) if *last == part => queued.push_str(&text),
            _ => self.backlog.push_back(Chunk::Text(part, text)),
        }
    }

    /// Hidden reasoning ends the text before it, as shown reasoning would,
    /// so the spinner has a line of its own.
    fn think(&mut self) {
        if self.activity == Activity::Thinking {
            return;
        }
        self.activity = Activity::Thinking;
        // With nothing queued, ending the reply directly keeps a spinner
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
                    Chunk::End => 0,
                })
                .sum::<usize>()
                .div_ceil(CATCH_UP),
        }
    }

    /// Hands the reply up to `budget` characters of the backlog, in order.
    fn reveal(&mut self, mut budget: usize) {
        while let Some(chunk) = self.backlog.pop_front() {
            match chunk {
                Chunk::End => self.reply.finish(),
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

    fn spin(&mut self) {
        let elapsed = self.started.elapsed();
        let frame = (elapsed.as_millis() / SPINNER_FRAME.as_millis()) as usize;
        let palette = theme::palette();
        let row = format!(
            "{} {}",
            palette.accent.paint(SPINNER[frame % SPINNER.len()]),
            palette.muted.paint(&format!("{} {}", self.activity.label(), clock(elapsed))),
        );
        if self.spinner.as_ref() != Some(&row) {
            show(format!("\r{}{row}", Clear(ClearType::CurrentLine)));
            self.spinner = Some(row);
        }
    }

    fn hide_spinner(&mut self) {
        if self.spinner.take().is_some() {
            show(format!("\r{}", Clear(ClearType::CurrentLine)));
        }
    }
}

fn clock(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds => format!("{}m {:02}s", seconds / 60, seconds % 60),
    }
}

