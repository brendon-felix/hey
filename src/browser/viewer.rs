/* -------------------------------------------------------------------------- */
/*                              browser/viewer.rs                             */
/* -------------------------------------------------------------------------- */

//! A conversation full-screen, entry by entry, kept up as its run goes on:
//! thinking shows as a preview, tool calls and subagents as a line each,
//! and any of them unfolds. A subagent's card opens its own conversation.

use std::collections::HashSet;
use std::ops::Range;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::{FutureExt, StreamExt};
use textwrap::core::display_width;
use tokio::time::{Instant, MissedTickBehavior, interval};
use yansi::Paint;

use rig_core::message::Message;

use super::entries::Transcript;
use crate::transcript;
use super::{Note, SPINNER};
use crate::backend::{ActivityStream, Backend, Events};
use crate::protocol::{Activities, AgentId, ApprovalId, Event, RunId, ThreadStatus};
use crate::render::{Style, palette};
use crate::sessions::SessionId;
use crate::ui::Screen;

const FRAME: Duration = Duration::from_millis(80);
/// How often a subagent's conversation is fetched again while it changes.
const AGENT_REFRESH: Duration = Duration::from_millis(300);
/// Events taken in before the screen is drawn again, so a run's replay
/// shows at once rather than an event a frame.
const BATCH: usize = 2000;
/// Rows above and below the conversation: the title and the keys.
const CHROME: usize = 2;
/// Messages of the thread loaded when it opens, and with each step back.
const WINDOW: usize = 80;
/// Messages loaded at most, before the far end unloads.
const MAX_LOADED: usize = WINDOW * 3;
/// Entries nearer the view than this stay loaded; load more nearer than it.
const KEEP_NEAR: usize = 20;

/// How the viewer was closed.
pub enum Closed {
    Back,
    /// To continue the conversation in this terminal.
    Open,
}

/// Shows `thread`, titled `title`, full-screen until closed: opened on its
/// subagent `agent`, if one is named, which goes back to the thread.
pub async fn view(
    backend: &Backend,
    thread: &SessionId,
    title: &str,
    agent: Option<AgentId>,
    style: &Style,
) -> Result<()> {
    super::ensure_terminal("the conversation viewer")?;
    let mut viewer = Viewer::new(backend, thread, title, style, false).await?;
    if let Some(agent) = agent {
        viewer.open_agent(agent).await;
    }
    let _screen = Screen::enter()?;
    viewer.run(&mut EventStream::new()).await.map(drop)
}

/// Shows `thread` on a screen already entered, reading keys from `input`;
/// `o` asks for it to be opened in the session.
pub(super) async fn show(
    backend: &Backend,
    thread: &SessionId,
    title: &str,
    style: &Style,
    input: &mut EventStream,
) -> Result<Closed> {
    let mut viewer = Viewer::new(backend, thread, title, style, true).await?;
    viewer.run(input).await
}

struct Viewer<'a> {
    backend: &'a Backend,
    thread: SessionId,
    title: String,
    style: Style,
    /// Whether `o` opens the conversation in the session.
    openable: bool,
    /// The thread, then the subagents opened from it, the last shown.
    pages: Vec<Page>,
    /// The thread's run, while one goes on.
    run: Option<Events>,
    /// The thread's latest run known, to tell when another starts.
    last_run: Option<RunId>,
    activity: Option<ActivityStream>,
    asking: HashSet<ApprovalId>,
    fetched: Instant,
    note: Option<Note>,
    frame: usize,
    /// As last drawn, for keys to move through.
    layout: Layout,
    painted: Vec<String>,
}

struct Page {
    /// The saved messages of the thread, of which only those from `start`
    /// are in `transcript`: more load as the view nears its top, and the
    /// far end unloads as it moves on. Empty for a subagent.
    messages: Vec<Message>,
    start: usize,
    subject: Subject,
    transcript: Transcript,
    place: Place,
    /// Whether what is shown may be behind what the host has.
    stale: bool,
}

enum Subject {
    Thread,
    Agent {
        agent: AgentId,
        label: String,
        working: bool,
    },
}

/// Where the view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// At the end, following what is added.
    Latest,
    /// On an entry, with the view's first row.
    At { selected: usize, top: usize },
    /// On an entry, with the view brought to its first row.
    Reveal(usize),
}

/// Rows of the page shown, and the rows each entry takes.
#[derive(Default)]
struct Layout {
    rows: Vec<String>,
    spans: Vec<Range<usize>>,
    /// Rows the view has room for.
    height: usize,
}

impl Layout {
    fn bottom(&self) -> usize {
        self.rows.len().saturating_sub(self.height)
    }

    /// The entry the row `row` is in, or the first after it.
    fn entry_at(&self, row: usize) -> Option<usize> {
        let found = self.spans.iter().position(|span| span.end > row);
        found.or(self.spans.len().checked_sub(1))
    }
}

impl<'a> Viewer<'a> {
    async fn new(
        backend: &'a Backend,
        thread: &SessionId,
        title: &str,
        style: &Style,
        openable: bool,
    ) -> Result<Self> {
        let mut activity = backend.activity().await.ok();
        let baseline = match &mut activity {
            Some(activity) => tokio::time::timeout(Duration::from_secs(2), activity.next())
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let last_run = baseline
            .and_then(Result::ok)
            .and_then(|activities| activities.get(thread).and_then(|activity| activity.run));
        let mut viewer = Self {
            backend,
            thread: thread.clone(),
            title: title.to_owned(),
            style: style.clone(),
            openable,
            pages: vec![Page::new(Subject::Thread)],
            run: None,
            last_run,
            activity,
            asking: HashSet::new(),
            fetched: Instant::now(),
            note: None,
            frame: 0,
            layout: Layout::default(),
            painted: Vec::new(),
        };
        viewer.reload().await?;
        Ok(viewer)
    }

    async fn run(&mut self, input: &mut EventStream) -> Result<Closed> {
        let mut frames = interval(FRAME);
        frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            // What a run streams shows with the next frame, so a reply is
            // not laid out again for every token.
            let redraw = tokio::select! {
                _ = frames.tick() => {
                    self.frame = self.frame.wrapping_add(1);
                    self.refresh_due().await;
                    true
                }
                received = next(&mut self.run) => {
                    self.follow(received).await;
                    false
                }
                activities = next(&mut self.activity) => {
                    self.watched(activities).await;
                    true
                }
                input = input.next() => match input {
                    Some(Ok(Input::Key(key))) if key.kind != KeyEventKind::Release => {
                        if let Some(closed) = self.key(key).await {
                            return Ok(closed);
                        }
                        true
                    }
                    Some(Ok(_)) => {
                        Vec::clear(&mut self.painted);
                        true
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(Closed::Back),
                },
            };
            if redraw {
                self.draw()?;
            }
        }
    }

    /// Opens the thread again, as it is now, keeping what was unfolded.
    async fn reload(&mut self) -> Result<()> {
        let opened = self.backend.view(&self.thread).await?;
        let messages = opened.snapshot.messages;
        let root = &mut self.pages[0];
        // The window stays where it was while the thread only grew.
        let kept = root.start > 0 && messages.len() >= root.messages.len();
        root.start = match kept {
            true => root.start,
            false => transcript::horizon(&messages, WINDOW),
        };
        let mut transcript = Transcript::saved(&messages[root.start..]);
        if let Some(running) = &opened.snapshot.running {
            transcript.user(&running.prompt);
        }
        transcript.unfold_as(&root.transcript);
        root.transcript = transcript;
        root.messages = messages;
        self.run = opened.events;
        HashSet::clear(&mut self.asking);
        if self.run.is_none() {
            self.viewed().await;
        }
        Ok(())
    }

    /// Takes in what the run did, and whatever else has arrived since.
    async fn follow(&mut self, received: Option<Result<Event>>) {
        let mut received = Some(received);
        for _ in 0..BATCH {
            let Some(next) = received.take() else {
                break;
            };
            self.take(next);
            received = self.run.as_mut().and_then(|run| run.next().now_or_never());
        }
        if self.run.is_none() {
            self.pages[0].transcript.settle();
            self.viewed().await;
        }
    }

    fn take(&mut self, received: Option<Result<Event>>) {
        let event = match received {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                self.note = Some(Note::failed(error.context("stopped following the answer")));
                self.run = None;
                return;
            }
            None => {
                self.run = None;
                return;
            }
        };
        match &event {
            Event::Began { run, .. } => self.last_run = Some(*run),
            Event::Approval { id, .. } => _ = self.asking.insert(*id),
            Event::Decided { id } => _ = self.asking.remove(id),
            Event::AgentStarted { agent, .. }
            | Event::Agent { agent, .. }
            | Event::AgentEnded { agent, .. } => self.stale_agents(Some(agent)),
            Event::End { .. } => {
                HashSet::clear(&mut self.asking);
                self.run = None;
                self.stale_agents(None);
            }
            _ => {}
        }
        self.pages[0].transcript.advance(&event);
    }

    /// Marks the pages of subagent `agent`, or of every subagent, as behind.
    fn stale_agents(&mut self, agent: Option<&AgentId>) {
        for page in &mut self.pages {
            if let Subject::Agent { agent: shown, .. } = &page.subject
                && agent.is_none_or(|agent| agent == shown)
            {
                page.stale = true;
            }
        }
    }

    /// Follows a run of the thread that started while none was followed.
    async fn watched(&mut self, activities: Option<Result<Activities>>) {
        let Some(Ok(activities)) = activities else {
            self.activity = None;
            return;
        };
        let Some(activity) = activities.get(&self.thread) else {
            return;
        };
        let running = matches!(
            activity.status,
            ThreadStatus::Running | ThreadStatus::Asking
        );
        if self.run.is_some()
            || activity.run.is_none()
            || (activity.run == self.last_run && !running)
        {
            return;
        }
        self.last_run = activity.run;
        if let Err(error) = self.reload().await {
            self.note = Some(Note::failed(error));
        }
        self.stale_agents(None);
    }

    async fn refresh_due(&mut self) {
        let due = self.fetched.elapsed() >= AGENT_REFRESH;
        if let Some(page) = self.pages.last()
            && page.stale
            && due
            && let Subject::Agent { agent, .. } = &page.subject
        {
            let agent = agent.clone();
            self.refresh_agent(&agent).await;
        }
    }

    /// Fetches subagent `agent`'s conversation for the page shown.
    async fn refresh_agent(&mut self, agent: &AgentId) {
        self.fetched = Instant::now();
        let fetched = self.backend.subagent(&self.thread, agent).await;
        let Some(page) = self.pages.last_mut() else {
            return;
        };
        page.stale = false;
        let snapshot = match fetched.and_then(|snapshot| {
            snapshot
                .context("the host no longer has this subagent: it keeps them until it restarts")
        }) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.note = Some(Note::failed(error));
                return;
            }
        };
        let mut transcript = Transcript::saved(&snapshot.messages);
        if let Some(working) = &snapshot.working {
            transcript.user(&working.message);
            working
                .steps
                .iter()
                .for_each(|step| transcript.advance(step));
        }
        transcript.unfold_as(&page.transcript);
        page.transcript = transcript;
        page.subject = Subject::Agent {
            agent: agent.clone(),
            label: snapshot.label,
            working: snapshot.working.is_some(),
        };
    }

    async fn open_agent(&mut self, agent: AgentId) {
        self.pages.push(Page::new(Subject::Agent {
            label: agent.to_string(),
            agent: agent.clone(),
            working: false,
        }));
        self.refresh_agent(&agent).await;
    }

    async fn viewed(&mut self) {
        if let Err(error) = self.backend.viewed(&self.thread).await {
            self.note = Some(Note::failed(error.context("still marked as new")));
        }
    }

    /// Handles a key; `Some` closes the viewer.
    async fn key(&mut self, key: KeyEvent) -> Option<Closed> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        self.note = None;
        match key.code {
            KeyCode::Char('c') if control => return Some(Closed::Back),
            KeyCode::Char('d') if control => self.scroll(self.layout.height as isize / 2),
            KeyCode::Char('u') if control => self.scroll(-(self.layout.height as isize / 2)),
            KeyCode::Char('q') => return Some(Closed::Back),
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => match self.pages.len() {
                1 => return Some(Closed::Back),
                _ => {
                    self.pages.pop();
                    self.stale_agents(None);
                }
            },
            KeyCode::Char('o') if self.openable && self.pages.len() == 1 => {
                return Some(Closed::Open);
            }
            KeyCode::Down | KeyCode::Char('j') => self.down(),
            KeyCode::Up | KeyCode::Char('k') => self.up(),
            KeyCode::PageDown | KeyCode::Char('f') => self.scroll(self.layout.height as isize),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-(self.layout.height as isize)),
            KeyCode::Home | KeyCode::Char('g') => {
                self.page().load_earlier(None);
                self.page().place = Place::At {
                    selected: 0,
                    top: 0,
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.page().place = Place::Latest;
                let page = self.page();
                // Back at the end, what is far from it unloads.
                if !page.messages.is_empty() {
                    let at = transcript::horizon(&page.messages, WINDOW);
                    if at > page.start {
                        page.transcript = Transcript::saved(&page.messages[at..]);
                        page.start = at;
                    }
                }
            }
            KeyCode::Char(' ') | KeyCode::Tab => self.toggle(),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.selected_agent() {
                Some(agent) => self.open_agent(agent).await,
                None => self.toggle(),
            },
            KeyCode::Char('e') => self.unfold_all(true),
            KeyCode::Char('c') => self.unfold_all(false),
            _ => {}
        }
        None
    }

    fn page(&mut self) -> &mut Page {
        let last = self.pages.len() - 1;
        &mut self.pages[last]
    }

    fn selected_agent(&mut self) -> Option<AgentId> {
        let (selected, _) = self.position()?;
        self.page().transcript.agent(selected).cloned()
    }

    /// The selected entry and the view's first row, as last drawn.
    fn position(&self) -> Option<(usize, usize)> {
        let page = self.pages.last()?;
        let last = self.layout.spans.len().checked_sub(1)?;
        Some(match page.place {
            Place::Latest => (last, self.layout.bottom()),
            Place::At { selected, top } => (selected.min(last), top.min(self.layout.bottom())),
            Place::Reveal(selected) => {
                let selected = selected.min(last);
                (
                    selected,
                    self.layout.spans[selected].start.min(self.layout.bottom()),
                )
            }
        })
    }

    /// To the next entry, through the rest of a tall one first.
    fn down(&mut self) {
        let Some((mut selected, mut top)) = self.position() else {
            return;
        };
        let (height, spans) = (self.layout.height, &self.layout.spans);
        let span = &spans[selected];
        if span.end > top + height {
            top = (top + height / 2).min(span.end - height);
        } else if selected + 1 < spans.len() {
            selected += 1;
            let span = &spans[selected];
            top = top.max(span.start.min(span.end.saturating_sub(height)));
        }
        let at_end = selected + 1 == spans.len() && top >= self.layout.bottom();
        self.page().place = match at_end {
            true => Place::Latest,
            false => Place::At { selected, top },
        };
    }

    /// To the entry before, through the rest of a tall one first.
    fn up(&mut self) {
        let Some((mut selected, mut top)) = self.position() else {
            return;
        };
        let (height, spans) = (self.layout.height, &self.layout.spans);
        let span = &spans[selected];
        if span.start < top {
            top = top.saturating_sub(height / 2).max(span.start);
        } else if selected > 0 {
            selected -= 1;
            let span = &spans[selected];
            if span.start < top {
                top = span.start.max(span.end.saturating_sub(height));
            }
        }
        self.page().place = Place::At { selected, top };
    }

    /// Moves the view `by` rows, selecting the first entry in it.
    fn scroll(&mut self, by: isize) {
        let Some((_, top)) = self.position() else {
            return;
        };
        let top = top.saturating_add_signed(by).min(self.layout.bottom());
        let Some(selected) = self.layout.entry_at(top) else {
            return;
        };
        self.page().place = match top >= self.layout.bottom() && by > 0 {
            true => Place::Latest,
            false => Place::At { selected, top },
        };
    }

    fn toggle(&mut self) {
        let Some((selected, top)) = self.position() else {
            return;
        };
        let start = self.layout.spans[selected].start;
        let page = self.page();
        if let Some(entry) = page.transcript.entry(selected) {
            entry.set_expanded(!entry.expanded());
        }
        page.place = Place::At {
            selected,
            top: top.min(start),
        };
    }

    fn unfold_all(&mut self, expanded: bool) {
        let selected = self.position().map(|(selected, _)| selected);
        let page = self.page();
        page.transcript
            .entries()
            .for_each(|entry| entry.set_expanded(expanded));
        if let Some(selected) = selected {
            page.place = Place::Reveal(selected);
        }
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = super::size()?;
        let spinner = SPINNER[self.frame % SPINNER.len()];
        let style = self.style.clone();
        let page = self.page();
        let mut layout = layout(
            &mut page.transcript,
            width.saturating_sub(2),
            &style,
            spinner,
        );
        layout.height = height.saturating_sub(CHROME).max(1);
        self.layout = layout;
        let position = self.position();
        let page = self.page();
        if let (Some((selected, top)), Place::Reveal(_)) = (position, page.place) {
            page.place = Place::At { selected, top };
        }
        // Load earlier messages as the view nears the top, drop the oldest
        // once it is far from them; the next frame shows the change.
        if let Some((selected, _)) = position
            && page.place != Place::Latest
            && !page.messages.is_empty()
        {
            if selected < KEEP_NEAR {
                page.load_earlier(Some(WINDOW));
            } else {
                page.unload_far(selected);
            }
        }

        let (selected, top) = position.map_or((None, 0), |(selected, top)| (Some(selected), top));
        let marked = selected.map_or(0..0, |selected| self.layout.spans[selected].clone());
        let mut lines = vec![self.header(width)];
        lines.extend(
            self.layout
                .rows
                .iter()
                .enumerate()
                .skip(top)
                .take(self.layout.height)
                .map(|(row, line)| {
                    let mark = match marked.contains(&row) {
                        true => palette().accent.paint("▌ ").to_string(),
                        false => "  ".to_owned(),
                    };
                    mark + &clipped(line, width.saturating_sub(2))
                }),
        );
        if self.layout.spans.is_empty() {
            lines.push(format!("  {}", palette().muted.paint("Nothing said yet.")));
        }
        let footer = self.footer(width);
        let mut frame = lines.clone();
        frame.push(footer.clone());
        if frame != self.painted {
            super::paint(lines, footer, height)?;
            self.painted = frame;
        }
        Ok(())
    }

    fn header(&self, width: usize) -> String {
        let spinner = SPINNER[self.frame % SPINNER.len()];
        let page = &self.pages[self.pages.len() - 1];
        let (view, working) = match &page.subject {
            Subject::Thread => (format!("“{}”", self.title), self.run.is_some()),
            Subject::Agent { label, working, .. } => {
                (format!("Subagent “{label}” of “{}”", self.title), *working)
            }
        };
        let mut tallies = Vec::new();
        if working {
            tallies.push(
                palette()
                    .accent
                    .paint(&format!("{spinner} working"))
                    .to_string(),
            );
        }
        if !self.asking.is_empty() {
            let close = if self.openable {
                "open it"
            } else {
                "close this"
            };
            tallies.push(
                format!("? waits for approval: {close} to answer")
                    .yellow()
                    .to_string(),
            );
        }
        if let Some((selected, _)) = self.position()
            && page.place != Place::Latest
        {
            tallies.push(
                palette()
                    .muted
                    .paint(&format!("{}/{}", selected + 1, self.layout.spans.len()))
                    .to_string(),
            );
        }
        super::header(self.backend, &view, &tallies, width)
    }

    fn footer(&mut self, width: usize) -> String {
        let mut keys = vec!["↑↓ move"];
        let selected = self.position().map(|(selected, _)| selected);
        let agent = self.selected_agent().is_some();
        let page = self.page();
        if let Some(entry) = selected.and_then(|selected| page.transcript.entry(selected))
            && entry.expandable()
        {
            keys.push(if entry.expanded() {
                "space fold"
            } else {
                "space unfold"
            });
        }
        if agent {
            keys.push("⏎ open subagent");
        }
        keys.push("e/c unfold/fold all");
        if page.place != Place::Latest {
            keys.push("G latest");
        }
        if self.openable && self.pages.len() == 1 {
            keys.push("o open here");
        }
        keys.push(if self.pages.len() > 1 {
            "esc back"
        } else {
            "q close"
        });
        let keys = format!(" {}", keys.join(" · "));
        super::footer(self.note.as_ref(), &keys, width)
    }
}

impl Page {
    /// Loads the messages before those shown, `more` of them, or all.
    fn load_earlier(&mut self, more: Option<usize>) {
        if self.start == 0 {
            return;
        }
        let to = match more {
            Some(more) => transcript::earlier(&self.messages, self.start.saturating_sub(more), more),
            None => 0,
        };
        if to >= self.start {
            return;
        }
        let earlier = Transcript::saved(&self.messages[to..self.start]);
        let added = earlier.len();
        self.transcript.prepend(earlier);
        self.start = to;
        self.shift(added as isize);
    }

    /// Unloads the oldest messages while the view is far from them.
    fn unload_far(&mut self, selected: usize) {
        let loaded = self.messages.len() - self.start;
        if loaded <= MAX_LOADED {
            return;
        }
        let to = transcript::earlier(&self.messages, self.start + WINDOW, WINDOW);
        let dropped = Transcript::saved(&self.messages[self.start..to]).len();
        if to <= self.start || selected < dropped + KEEP_NEAR {
            return;
        }
        self.transcript.drop_front(dropped);
        self.start = to;
        self.shift(-(dropped as isize));
    }

    /// Keeps the view on its entry after `by` entries came or went before it.
    fn shift(&mut self, by: isize) {
        self.place = match self.place {
            Place::Latest => Place::Latest,
            Place::At { selected, .. } | Place::Reveal(selected) => {
                Place::Reveal(selected.saturating_add_signed(by))
            }
        };
    }

    fn new(subject: Subject) -> Self {
        Self {
            messages: Vec::new(),
            start: 0,
            subject,
            transcript: Transcript::default(),
            place: Place::Latest,
            stale: true,
        }
    }
}

/// The next item of `stream`; with none, never.
async fn next<T>(stream: &mut Option<futures::stream::BoxStream<'static, T>>) -> Option<T> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// `transcript`'s rows, `width` columns wide, a blank row between entries
/// but for those of one line each.
fn layout(transcript: &mut Transcript, width: usize, style: &Style, spinner: &str) -> Layout {
    let mut layout = Layout::default();
    let mut previous: Option<bool> = None;
    for entry in transcript.entries() {
        let compact = entry.compact();
        if previous.is_some_and(|previous| !(previous && compact)) {
            layout.rows.push(String::new());
        }
        let start = layout.rows.len();
        layout.rows.extend(entry.rows(width, style, spinner));
        layout.spans.push(start..layout.rows.len());
        previous = Some(compact);
    }
    layout
}

/// `row` cut to `width` columns, its colour codes kept, so none wraps.
fn clipped(row: &str, width: usize) -> String {
    let mut kept = String::new();
    let mut used = 0;
    let mut chars = row.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            kept.push(c);
            for next in chars.by_ref() {
                kept.push(next);
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let c = if c.is_control() { ' ' } else { c };
        let taken = display_width(c.encode_utf8(&mut [0; 4]));
        if used + taken > width {
            kept.push_str("\u{1b}[0m");
            break;
        }
        used += taken;
        kept.push(c);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_keeps_colours_and_counts_only_what_shows() {
        let red = "\u{1b}[31mabcdef\u{1b}[0m";
        let cut = clipped(red, 3);
        assert_eq!(cut, "\u{1b}[31mabc\u{1b}[0m");
        assert_eq!(clipped("日本語", 4), "日本\u{1b}[0m");
        assert_eq!(clipped("short", 10), "short");
    }
}
