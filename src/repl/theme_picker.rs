/* -------------------------------------------------------------------------- */
/*                            repl/theme_picker.rs                            */
/* -------------------------------------------------------------------------- */

//! Picks a theme while previewing it: as each theme is selected, a sample
//! conversation and the picker itself are drawn in its colours.

use std::collections::HashMap;
use std::io::{IsTerminal, Write, stdin, stdout};
use std::sync::Arc;

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use crossterm::{cursor, queue};
use dialoguer::theme::Theme as _;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::render::{self, Theme};
use crate::ui::{self, Screen};

const PROMPT: &str = "Select a theme";
const KEYS: &str = "↑↓ preview · ⏎ choose · esc cancel · type to filter";
const LIST_ROWS: usize = 10;

const QUESTION: &str = "Show me a factorial in Rust and an API client in Python";

pub const SAMPLE: &str = r#"# Sample text with code:

- Rust **function** for computing _factorial_:

    ```rust
    fn factorial(n: u32) -> u32 {
        if n == 0 { 1 } else { n * factorial(n - 1) }
    }
    ```
- Python **class** for an _API client_:

    ```python
    class APIClient:
        def __init__(self, api_key):
            self.api_key = api_key
    ```
"#;

/// Picks one of the themes, starting at the current one; `None` when
/// cancelled with Esc or Ctrl+C.
pub fn pick() -> Result<Option<String>> {
    if !stdout().is_terminal() || !stdin().is_terminal() {
        bail!("the theme picker needs a terminal");
    }
    let current = render::current_theme();
    let names = render::themes();
    let selected = names.iter().position(|name| *name == current.name).or(Some(0));
    let mut picker = Picker {
        names,
        filter: String::new(),
        selected,
        scroll: 0,
        loaded: HashMap::new(),
        current,
        matcher: SkimMatcherV2::default(),
    };
    let _screen = Screen::enter()?;
    picker.run()
}

struct Picker {
    names: Vec<String>,
    filter: String,
    /// An index into `names`, so it stays put as the filter changes.
    selected: Option<usize>,
    scroll: usize,
    loaded: HashMap<usize, Arc<Theme>>,
    /// What is previewed while nothing matches the filter.
    current: Arc<Theme>,
    matcher: SkimMatcherV2,
}

impl Picker {
    fn run(&mut self) -> Result<Option<String>> {
        loop {
            self.draw()?;
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && let Some(picked) = self.key(key)
            {
                return Ok(picked.map(|index| self.names[index].clone()));
            }
        }
    }

    /// Handles a key; `Some` closes the picker, choosing the theme it holds.
    fn key(&mut self, key: KeyEvent) -> Option<Option<usize>> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => return Some(None),
            KeyCode::Esc => return Some(None),
            KeyCode::Enter => return self.selected.map(Some),
            KeyCode::Up | KeyCode::BackTab => self.step(-1),
            KeyCode::Char('p') if control => self.step(-1),
            KeyCode::Down | KeyCode::Tab => self.step(1),
            KeyCode::Char('n') if control => self.step(1),
            KeyCode::Backspace => self.refilter(|filter| _ = filter.pop()),
            KeyCode::Char(c) if !control => self.refilter(|filter| filter.push(c)),
            _ => {}
        }
        None
    }

    /// Changes the filter and selects its best match.
    fn refilter(&mut self, change: impl FnOnce(&mut String)) {
        change(&mut self.filter);
        self.selected = self.shown().first().copied();
        self.scroll = 0;
    }

    /// Indexes of the themes matching the filter, best first.
    fn shown(&self) -> Vec<usize> {
        let mut scored: Vec<_> = self
            .names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| Some((index, self.matcher.fuzzy_match(name, &self.filter)?)))
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        scored.into_iter().map(|(index, _)| index).collect()
    }

    /// Moves the selection by `by`, wrapping around the ends.
    fn step(&mut self, by: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let at = self.position(&shown) as isize;
        let to = (at + by).rem_euclid(shown.len() as isize) as usize;
        self.selected = Some(shown[to]);
    }

    fn position(&self, shown: &[usize]) -> usize {
        shown.iter().position(|index| Some(*index) == self.selected).unwrap_or(0)
    }

    fn previewed(&mut self) -> Result<Arc<Theme>> {
        let Some(index) = self.selected else {
            return Ok(self.current.clone());
        };
        if let Some(theme) = self.loaded.get(&index) {
            return Ok(theme.clone());
        }
        let theme = Arc::new(Theme::load(&self.names[index])?);
        self.loaded.insert(index, theme.clone());
        Ok(theme)
    }

    /// The sample conversation, then the picker under it, all in the selected
    /// theme's colours.
    fn draw(&mut self) -> Result<()> {
        let (width, height) = terminal::size().map(|(w, h)| (usize::from(w), usize::from(h)))?;
        let theme = self.previewed()?;
        let mut lines = sample(&theme, width);
        let picker = self.picker(&theme)?;
        lines.truncate(height.saturating_sub(picker.len()));
        lines.extend(picker);
        lines.resize(height, String::new());

        let mut out = stdout().lock();
        queue!(out, BeginSynchronizedUpdate)?;
        for (row, line) in lines.iter().enumerate() {
            queue!(out, cursor::MoveTo(0, row as u16), Print(line), Clear(ClearType::UntilNewLine))?;
        }
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        Ok(())
    }

    /// The prompt and themes as dialoguer's fuzzy select draws them, and the
    /// keys.
    fn picker(&mut self, theme: &Theme) -> Result<Vec<String>> {
        let style = ui::theme_in(theme.palette);
        let shown = self.shown();
        let at = self.position(&shown);
        let rows = LIST_ROWS.min(shown.len());
        self.scroll = self.scroll.min(at).max((at + 1).saturating_sub(rows));

        let mut lines = vec![String::new()];
        style.format_fuzzy_select_prompt(&mut lines[0], PROMPT, &self.filter, self.filter.len())?;
        for index in shown.iter().skip(self.scroll).take(rows) {
            let mut line = String::new();
            let active = Some(*index) == self.selected;
            let name = &self.names[*index];
            style.format_fuzzy_select_prompt_item(&mut line, name, active, true, &self.matcher, &self.filter)?;
            lines.push(line);
        }
        lines.push(String::new());
        lines.push(theme.palette.muted.paint(KEYS).to_string());
        Ok(lines)
    }
}

/// A question and its answer as the REPL draws them.
fn sample(theme: &Arc<Theme>, width: usize) -> Vec<String> {
    let mut lines = vec![String::new(), render::user_line(QUESTION, theme.palette), String::new()];
    lines.extend(render::reply_rows(SAMPLE, theme.clone(), width));
    lines.push(String::new());
    lines
}
