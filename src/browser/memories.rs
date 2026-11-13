/* -------------------------------------------------------------------------- */
/*                            browser/memories.rs                             */
/* -------------------------------------------------------------------------- */

//! What a host remembers in a full-screen list, newest first, and keys to
//! write, reword and forget memories.

use std::ops::ControlFlow;

use anyhow::{Result, anyhow};
use crossterm::event::{Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use yansi::Paint;

use super::field::Field;
use super::{Note, REFRESH, fit};
use crate::backend::Backend;
use crate::memory;
use crate::protocol::Revision;
use crate::render::palette;
use crate::ui::Screen;

/// Lists the host's memories to change them until the list is closed.
pub async fn browse(backend: &Backend) -> Result<()> {
    super::ensure_terminal("the memory list")?;
    let mut browser = Browser {
        backend,
        memories: backend.memories().await?,
        filter: String::new(),
        mode: Mode::Browse,
        selected: None,
        scroll: 0,
        note: None,
    };
    let _screen = Screen::enter()?;
    browser.run().await
}

struct Browser<'a> {
    backend: &'a Backend,
    /// Oldest first, as saved.
    memories: Vec<String>,
    filter: String,
    mode: Mode,
    /// Kept by text, so it stays put as the list refreshes.
    selected: Option<String>,
    scroll: usize,
    note: Option<Note>,
}

enum Mode {
    Browse,
    Filter,
    Edit(Draft),
    ConfirmForget(String),
}

struct Draft {
    target: Target,
    field: Field,
}

enum Target {
    New,
    Revise(String),
}

impl Browser<'_> {
    async fn run(&mut self) -> Result<()> {
        let mut input = EventStream::new();
        let mut refreshes = interval_at(Instant::now() + REFRESH, REFRESH);
        refreshes.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            self.draw()?;
            tokio::select! {
                // Not while editing, so the memory being edited stays in view.
                _ = refreshes.tick(), if !matches!(self.mode, Mode::Edit(_)) => self.refresh().await,
                input = input.next() => match input {
                    Some(Ok(Input::Key(key))) if key.kind != KeyEventKind::Release => {
                        if self.key(key).await.is_break() {
                            return Ok(());
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                },
            }
        }
    }

    async fn key(&mut self, key: KeyEvent) -> ControlFlow<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return ControlFlow::Break(());
        }
        self.note = None;
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::ConfirmForget(memory) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.forget(memory).await;
                }
            }
            Mode::Edit(mut draft) => match key.code {
                KeyCode::Enter => self.save(draft).await,
                KeyCode::Esc => {}
                _ => {
                    draft.field.key(key);
                    self.mode = Mode::Edit(draft);
                }
            },
            Mode::Filter => match key.code {
                KeyCode::Enter => self.edit(),
                KeyCode::Esc => String::clear(&mut self.filter),
                KeyCode::Up => self.filtering(|browser| browser.step(-1)),
                KeyCode::Down => self.filtering(|browser| browser.step(1)),
                KeyCode::Backspace => self.filtering(|browser| _ = browser.filter.pop()),
                KeyCode::Char(c) => self.filtering(|browser| {
                    browser.filter.push(c);
                    browser.selected = browser.shown().first().map(|i| browser.memories[*i].clone());
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
                KeyCode::Enter | KeyCode::Char('e') => self.edit(),
                KeyCode::Char('n') => {
                    self.mode = Mode::Edit(Draft {
                        target: Target::New,
                        field: Field::new(""),
                    });
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(memory) = self.selected() {
                        self.mode = Mode::ConfirmForget(memory.clone());
                    }
                }
                KeyCode::Char('/') => self.mode = Mode::Filter,
                KeyCode::Char('r') => self.refresh().await,
                KeyCode::Esc if !self.filter.is_empty() => String::clear(&mut self.filter),
                KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
                _ => {}
            },
        }
        ControlFlow::Continue(())
    }

    fn filtering(&mut self, change: impl FnOnce(&mut Self)) {
        change(self);
        self.mode = Mode::Filter;
    }

    /// Indexes of the memories in view, newest first, matching the filter.
    fn shown(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.memories.len())
            .rev()
            .filter(|i| self.memories[*i].to_lowercase().contains(&filter))
            .collect()
    }

    /// Where the selection is among `shown`, the first when it is not there.
    fn position(&self, shown: &[usize]) -> usize {
        shown
            .iter()
            .position(|i| Some(&self.memories[*i]) == self.selected.as_ref())
            .unwrap_or(0)
    }

    fn selected(&self) -> Option<&String> {
        let shown = self.shown();
        shown.get(self.position(&shown)).map(|i| &self.memories[*i])
    }

    fn step(&mut self, by: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let to = (self.position(&shown) as isize).saturating_add(by);
        let to = to.clamp(0, shown.len() as isize - 1) as usize;
        self.selected = Some(self.memories[shown[to]].clone());
    }

    /// Moves the selection off a memory about to leave the view: to the one
    /// after it, else the one before.
    fn step_off(&mut self) {
        let shown = self.shown();
        let at = self.position(&shown);
        let next = shown.get(at + 1).or_else(|| at.checked_sub(1).and_then(|before| shown.get(before)));
        self.selected = next.map(|i| self.memories[*i].clone());
    }

    fn edit(&mut self) {
        if let Some(memory) = self.selected() {
            self.mode = Mode::Edit(Draft {
                field: Field::new(memory),
                target: Target::Revise(memory.clone()),
            });
        }
    }

    async fn refresh(&mut self) {
        match self.backend.memories().await {
            Ok(memories) => self.memories = memories,
            Err(error) => self.fail(error),
        }
    }

    /// Saves the draft, or keeps editing it when that fails.
    async fn save(&mut self, draft: Draft) {
        let text = memory::one_line(draft.field.text());
        if text.is_empty() {
            self.fail(anyhow!("a memory cannot be empty; esc cancels"));
            self.mode = Mode::Edit(draft);
            return;
        }
        let saved = match &draft.target {
            Target::Revise(old) if *old == text => Ok(None),
            Target::Revise(old) => {
                let revision = Revision {
                    old: old.clone(),
                    new: text.clone(),
                };
                self.backend.revise(&revision).await.map(|()| Some("Saved"))
            }
            Target::New => self.backend.remember(&text).await.map(|added| match added {
                true => Some("Remembered"),
                false => Some("Already remembered"),
            }),
        };
        match saved {
            Ok(said) => {
                if let Some(said) = said {
                    self.say(format!("{said} “{text}”"));
                }
                self.selected = Some(text);
                self.refresh().await;
            }
            Err(error) => {
                self.fail(error);
                self.mode = Mode::Edit(draft);
            }
        }
    }

    async fn forget(&mut self, memory: String) {
        match self.backend.forget(&memory).await {
            Ok(forgotten) => {
                self.say(match forgotten {
                    true => format!("Forgot “{memory}”"),
                    false => "Already forgotten".into(),
                });
                self.step_off();
                self.refresh().await;
            }
            Err(error) => self.fail(error),
        }
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
        let rows = super::page();
        let filtering = matches!(self.mode, Mode::Filter);
        let mut lines = vec![self.header(width), super::filter_line(filtering, &self.filter)];
        match &self.mode {
            Mode::Edit(Draft { target: Target::New, field }) => {
                self.scroll = 0;
                lines.push(editing(field, width));
                let below = shown.iter().take(rows.saturating_sub(1));
                lines.extend(below.map(|i| row(&self.memories[*i], false, width)));
            }
            mode => {
                let at = self.position(&shown);
                self.scroll = super::scrolled(self.scroll, at, rows);
                let visible = shown.iter().enumerate().skip(self.scroll).take(rows);
                lines.extend(visible.map(|(row_at, i)| match mode {
                    Mode::Edit(draft) if row_at == at => editing(&draft.field, width),
                    _ => row(&self.memories[*i], row_at == at, width),
                }));
                if shown.is_empty() {
                    lines.push(format!("  {}", palette().muted.paint(&self.empty())));
                }
            }
        }
        super::paint(lines, self.footer(width), height)
    }

    fn header(&self, width: usize) -> String {
        let tally = match self.memories.len() {
            1 => "1 memory".to_owned(),
            count => format!("{count} memories"),
        };
        super::header(self.backend, "Memories", &[palette().muted.paint(&tally).to_string()], width)
    }

    fn empty(&self) -> String {
        match self.filter.is_empty() {
            false => format!("Nothing matches “{}”.", self.filter),
            true => "Nothing remembered yet; n writes a memory.".into(),
        }
    }

    fn footer(&self, width: usize) -> String {
        let keys = match &self.mode {
            Mode::ConfirmForget(memory) => {
                let prompt = format!(" Forget “{memory}”? y forgets it, any other key keeps it");
                return super::question(&prompt, width);
            }
            Mode::Edit(_) => " ⏎ save · esc cancel · ←→ move · ctrl+w erase word",
            Mode::Filter => " type to filter · ↑↓ move · ⏎ edit · esc clear",
            Mode::Browse => " ↑↓ move · ⏎ edit · n new · d forget · / filter · q quit",
        };
        super::footer(self.note.as_ref(), keys, width)
    }
}

fn row(memory: &str, selected: bool, width: usize) -> String {
    let text = fit(memory, width.saturating_sub(3));
    let gap = width.saturating_sub(3 + text.chars().count());
    let pointer = if selected { "›" } else { " " };
    let line = format!("{pointer} {text}{} ", " ".repeat(gap));
    match selected {
        true => line.invert().to_string(),
        false => line,
    }
}

fn editing(field: &Field, width: usize) -> String {
    format!("{} {}", palette().accent.paint("›"), field.view(width.saturating_sub(3)))
}
