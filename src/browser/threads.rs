/* -------------------------------------------------------------------------- */
/*                             browser/threads.rs                             */
/* -------------------------------------------------------------------------- */

//! A host's conversations in a full-screen list: what each is doing, and
//! keys to open, archive, delete and fork them.

use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, Local};
use crossterm::event::{Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};
use yansi::{Paint, Painted};

use super::{Note, REFRESH, fit};
use crate::backend::Backend;
use crate::protocol::{ThreadInfo, ThreadStatus};
use crate::render::palette;
use crate::sessions::SessionId;
use crate::ui::Screen;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME: Duration = Duration::from_millis(80);
const WHEN_WIDTH: usize = 11;

/// What the user did in the browser.
pub struct Browsed {
    /// The thread to open, if one was picked.
    pub open: Option<SessionId>,
    pub deleted: Vec<SessionId>,
}

/// Lists the host's conversations until one is picked or the list closed.
/// `current` is the one the REPL is in.
pub async fn browse(backend: &Backend, current: Option<&SessionId>) -> Result<Browsed> {
    super::ensure_terminal("the conversation list")?;
    let mut browser = Browser {
        backend,
        current: current.cloned(),
        threads: backend.threads().await?,
        archived: false,
        filter: String::new(),
        mode: Mode::Browse,
        selected: current.cloned(),
        scroll: 0,
        frame: 0,
        note: None,
        deleted: Vec::new(),
    };
    let _screen = Screen::enter()?;
    browser.run().await
}

struct Browser<'a> {
    backend: &'a Backend,
    current: Option<SessionId>,
    threads: Vec<ThreadInfo>,
    /// Whether the archived threads are shown, rather than the rest.
    archived: bool,
    filter: String,
    mode: Mode,
    /// Kept by id, so it stays put as the list refreshes.
    selected: Option<SessionId>,
    scroll: usize,
    frame: usize,
    note: Option<Note>,
    deleted: Vec<SessionId>,
}

enum Mode {
    Browse,
    Filter,
    ConfirmDelete(SessionId),
}

impl Browser<'_> {
    async fn run(&mut self) -> Result<Browsed> {
        let mut input = EventStream::new();
        let mut frames = interval(FRAME);
        let mut refreshes = interval_at(Instant::now() + REFRESH, REFRESH);
        frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
        refreshes.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            self.draw()?;
            tokio::select! {
                _ = frames.tick() => self.frame = self.frame.wrapping_add(1),
                _ = refreshes.tick() => self.refresh().await,
                input = input.next() => match input {
                    Some(Ok(Input::Key(key))) if key.kind != KeyEventKind::Release => {
                        if let Some(open) = self.key(key).await {
                            return Ok(self.finish(open));
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(self.finish(None)),
                },
            }
        }
    }

    fn finish(&mut self, open: Option<SessionId>) -> Browsed {
        Browsed {
            open,
            deleted: std::mem::take(&mut self.deleted),
        }
    }

    /// Handles a key; `Some` closes the browser, opening the thread it holds.
    async fn key(&mut self, key: KeyEvent) -> Option<Option<SessionId>> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(None);
        }
        self.note = None;
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::ConfirmDelete(id) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.delete(id).await;
                }
            }
            Mode::Filter => match key.code {
                KeyCode::Enter => return self.selected.clone().map(Some),
                KeyCode::Esc => String::clear(&mut self.filter),
                KeyCode::Up => self.filtering(|browser| browser.step(-1)),
                KeyCode::Down => self.filtering(|browser| browser.step(1)),
                KeyCode::Backspace => self.filtering(|browser| _ = browser.filter.pop()),
                KeyCode::Char(c) => self.filtering(|browser| {
                    browser.filter.push(c);
                    browser.selected = browser.shown().first().map(|i| browser.threads[*i].summary.id.clone());
                }),
                _ => self.mode = Mode::Filter,
            },
            Mode::Browse => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::PageUp => self.step(-(super::page() as isize)),
                KeyCode::PageDown => self.step(super::page() as isize),
                KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
                KeyCode::Enter => return self.selected().map(|thread| Some(thread.summary.id.clone())),
                KeyCode::Char('a') => self.archive().await,
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(thread) = self.selected() {
                        self.mode = Mode::ConfirmDelete(thread.summary.id.clone());
                    }
                }
                KeyCode::Char('f') => self.fork().await,
                KeyCode::Tab => {
                    self.archived = !self.archived;
                    self.selected = None;
                }
                KeyCode::Char('/') => self.mode = Mode::Filter,
                KeyCode::Char('r') => self.refresh().await,
                KeyCode::Esc if !self.filter.is_empty() => String::clear(&mut self.filter),
                KeyCode::Char('q') | KeyCode::Esc => return Some(None),
                _ => {}
            },
        }
        None
    }

    fn filtering(&mut self, change: impl FnOnce(&mut Self)) {
        change(self);
        self.mode = Mode::Filter;
    }

    /// Indexes of the threads in view: archived or not, matching the filter.
    fn shown(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, thread)| thread.archived == self.archived)
            .filter(|(_, thread)| thread.summary.title.to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect()
    }

    /// Where the selection is among `shown`, the first when it is not there.
    fn position(&self, shown: &[usize]) -> usize {
        shown
            .iter()
            .position(|index| Some(&self.threads[*index].summary.id) == self.selected.as_ref())
            .unwrap_or(0)
    }

    fn selected(&self) -> Option<&ThreadInfo> {
        let shown = self.shown();
        shown.get(self.position(&shown)).map(|index| &self.threads[*index])
    }

    fn step(&mut self, by: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let to = (self.position(&shown) as isize).saturating_add(by);
        let to = to.clamp(0, shown.len() as isize - 1) as usize;
        self.selected = Some(self.threads[shown[to]].summary.id.clone());
    }

    /// Moves the selection off a thread about to leave the view: to the one
    /// after it, else the one before.
    fn step_off(&mut self) {
        let shown = self.shown();
        let at = self.position(&shown);
        let next = shown.get(at + 1).or_else(|| at.checked_sub(1).and_then(|before| shown.get(before)));
        self.selected = next.map(|index| self.threads[*index].summary.id.clone());
    }

    async fn refresh(&mut self) {
        match self.backend.threads().await {
            Ok(threads) => self.threads = threads,
            Err(error) => self.fail(error),
        }
    }

    async fn archive(&mut self) {
        let Some(thread) = self.selected().cloned() else {
            return;
        };
        let archived = !thread.archived;
        match self.backend.archive(&thread.summary.id, archived).await {
            Ok(()) => {
                let title = &thread.summary.title;
                self.say(match archived {
                    true => format!("Archived “{title}” · Tab shows archived conversations"),
                    false => format!("Restored “{title}”"),
                });
                self.step_off();
                self.refresh().await;
            }
            Err(error) => self.fail(error),
        }
    }

    async fn delete(&mut self, id: SessionId) {
        let title = self.title(&id);
        match self.backend.delete(&id).await {
            Ok(()) => {
                self.say(format!("Deleted “{title}”"));
                self.step_off();
                self.deleted.push(id);
                self.refresh().await;
            }
            Err(error) => self.fail(error),
        }
    }

    async fn fork(&mut self) {
        let Some(thread) = self.selected().cloned() else {
            return;
        };
        match self.backend.fork(&thread.summary.id).await {
            Ok(copy) => {
                self.say(format!("Forked “{}”", thread.summary.title));
                self.archived = false;
                self.selected = Some(copy);
                self.refresh().await;
            }
            Err(error) => self.fail(error),
        }
    }

    fn title(&self, id: &SessionId) -> String {
        self.threads
            .iter()
            .find(|thread| &thread.summary.id == id)
            .map_or_else(|| id.to_string(), |thread| thread.summary.title.clone())
    }

    fn say(&mut self, text: String) {
        self.note = Some(Note::said(text));
    }

    fn fail(&mut self, error: anyhow::Error) {
        self.note = Some(Note::failed(error));
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = super::size()?;
        let shown = self.shown();
        let at = self.position(&shown);
        let rows = super::page();
        self.scroll = super::scrolled(self.scroll, at, rows);

        let filtering = matches!(self.mode, Mode::Filter);
        let mut lines = vec![self.header(width), super::filter_line(filtering, &self.filter)];
        lines.extend(shown.iter().enumerate().skip(self.scroll).take(rows).map(|(row, index)| {
            self.row(&self.threads[*index], row == at, width)
        }));
        if shown.is_empty() {
            lines.push(format!("  {}", palette().muted.paint(&self.empty())));
        }
        super::paint(lines, self.footer(width), height)
    }

    fn header(&self, width: usize) -> String {
        let view = if self.archived { "Archived conversations" } else { "Conversations" };
        let active = self.threads.iter().filter(|thread| !thread.archived);
        let count = |status: &[ThreadStatus]| active.clone().filter(|thread| status.contains(&thread.status)).count();
        let running = count(&[ThreadStatus::Running, ThreadStatus::Asking]);
        let new = count(&[ThreadStatus::Unseen]);
        let mut tallies = Vec::new();
        if running > 0 {
            tallies.push(palette().accent.paint(&format!("{} {running} running", self.spinner())).to_string());
        }
        if new > 0 {
            tallies.push(format!("● {new} new").blue().to_string());
        }
        super::header(self.backend, view, &tallies, width)
    }

    fn empty(&self) -> String {
        match (self.filter.is_empty(), self.archived) {
            (false, _) => format!("Nothing matches “{}”.", self.filter),
            (true, true) => "Nothing archived.".into(),
            (true, false) => "No conversations yet.".into(),
        }
    }

    fn row(&self, thread: &ThreadInfo, selected: bool, width: usize) -> String {
        let palette = palette();
        let current = self.current.as_ref() == Some(&thread.summary.id);
        let (glyph, label): (Painted<&str>, Painted<&str>) = match thread.status {
            ThreadStatus::Running => (palette.accent.paint(self.spinner()), palette.accent.paint("running")),
            ThreadStatus::Asking => ("?".yellow().bold(), "needs approval".yellow()),
            ThreadStatus::Unseen => ("●".blue(), "new".blue()),
            ThreadStatus::Idle if current => (" ".new(), palette.muted.paint("current")),
            ThreadStatus::Idle => (" ".new(), "".new()),
        };
        let when = format!("{:<WHEN_WIDTH$}", when(&thread.summary.id));
        let fixed = 1 + 1 + 2 + WHEN_WIDTH + 2 + 2 + label.value.chars().count() + 1;
        let title = fit(&thread.summary.title, width.saturating_sub(fixed));
        let gap = width.saturating_sub(fixed + title.chars().count());
        let pointer = if selected { "›" } else { " " };
        let padding = " ".repeat(gap + 2);
        let spans = [
            pointer.new(),
            glyph,
            "  ".new(),
            palette.muted.paint(when.as_str()),
            "  ".new(),
            title.as_str().new(),
            padding.as_str().new(),
            label,
            " ".new(),
        ];
        spans
            .into_iter()
            .map(|span| if selected { span.invert().to_string() } else { span.to_string() })
            .collect()
    }

    fn footer(&self, width: usize) -> String {
        let keys = match &self.mode {
            Mode::ConfirmDelete(id) => {
                let prompt = format!(" Delete “{}”? y deletes it, any other key keeps it", self.title(id));
                return super::question(&prompt, width);
            }
            Mode::Filter => " type to filter · ↑↓ move · ⏎ open · esc clear".into(),
            Mode::Browse => {
                let archive = if self.archived { "restore" } else { "archive" };
                let view = if self.archived { "conversations" } else { "archived" };
                format!(" ↑↓ move · ⏎ open · a {archive} · d delete · f fork · tab {view} · / filter · q quit")
            }
        };
        super::footer(self.note.as_ref(), &keys, width)
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.frame % SPINNER.len()]
    }
}

/// When a conversation started, as briefly as still tells it apart.
fn when(id: &SessionId) -> String {
    let Some(started) = id.started() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    match (today - started.date()).num_days() {
        0 => started.format("%H:%M").to_string(),
        1 => "yesterday".into(),
        2..=6 => started.format("%a %H:%M").to_string(),
        _ if started.year() == today.year() => started.format("%b %e").to_string(),
        _ => started.format("%Y-%m-%d").to_string(),
    }
}
