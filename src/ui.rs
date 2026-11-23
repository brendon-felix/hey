/* -------------------------------------------------------------------------- */
/*                                   ui.rs                                    */
/* -------------------------------------------------------------------------- */

use std::io::{self, ErrorKind, IsTerminal, Write, stdout};

use anyhow::{Result, bail};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{Event as Input, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute, queue, terminal};
use dialoguer::console::{self, Style};
use dialoguer::{FuzzySelect, Password, theme::ColorfulTheme};
use futures::StreamExt;
use yansi::Paint;

use crate::protocol::{Ask, Decision};
use crate::render::{self, Palette, palette};

fn theme() -> ColorfulTheme {
    theme_in(palette())
}

/// dialoguer's colourful theme in `palette`'s colours.
pub fn theme_in(palette: Palette) -> ColorfulTheme {
    let style = |color| tint(Style::new().for_stderr(), color);
    let mark = |text: &str, color| style(color).apply_to(text.to_owned());
    ColorfulTheme {
        defaults_style: style(palette.value),
        prompt_prefix: mark("?", palette.accent),
        prompt_suffix: mark("›", palette.muted),
        success_prefix: mark("✔", palette.input),
        success_suffix: mark("·", palette.muted),
        hint_style: style(palette.muted),
        values_style: style(palette.input),
        active_item_style: style(palette.name),
        active_item_prefix: mark("❯", palette.accent),
        picked_item_prefix: mark("❯", palette.accent),
        fuzzy_match_highlight_style: style(palette.name).bold(),
        ..ColorfulTheme::default()
    }
}

fn tint(style: Style, color: render::Color) -> Style {
    match color {
        render::Color::Default => style,
        render::Color::Ansi(index) => style.fg(console::Color::Color256(index)),
        render::Color::Rgb(r, g, b) => style.true_color(r, g, b),
    }
}

/// Asks `ask`. Esc declines; Ctrl+C stops the run. Dropped unanswered, as
/// when another client decides first, it takes the question off the screen.
pub async fn confirm(ask: &Ask, style: &render::Style) -> Decision {
    if !std::io::stdin().is_terminal() {
        return Decision::Stop;
    }
    let decisions = [Decision::Once, Decision::Always, Decision::Decline];
    let (question, labels) = match ask {
        Ask::Question { question, choices } => return answer(question, choices).await,
        Ask::Plan { plan } => return approve_plan(plan, style).await,
        Ask::Tool {
            name,
            agent,
            machine,
        } => {
            let on = machine
                .as_ref()
                .map(|machine| format!(" on {machine}"))
                .unwrap_or_default();
            let question = match agent {
                Some(agent) => format!("Run `{name}`{on} for agent “{agent}”?"),
                None => format!("Run `{name}`{on}?"),
            };
            let always = format!("Always allow `{name}`{on} in this conversation");
            (question, ["Yes".to_owned(), always, "No".to_owned()])
        }
        Ask::Directory { machine, path } => (
            format!("Let the agent use {path} on {machine}?"),
            [
                "Yes, in this conversation".to_owned(),
                format!("Always: allow it in {machine}'s config"),
                "No".to_owned(),
            ],
        ),
    };
    let answer = async { Menu::open(question, labels.into())?.answer().await };
    match answer.await {
        Ok(Answer::Picked(index)) => decisions[index].clone(),
        Ok(Answer::Dismissed) => Decision::Decline,
        Ok(Answer::Interrupted) | Err(_) => Decision::Stop,
    }
}

/// How a message typed during an answer should be sent: `Some(true)` to
/// steer the answer, `Some(false)` to queue it for after; `None` when
/// dismissed with Esc or Ctrl+C.
pub async fn steer_or_queue() -> Option<bool> {
    let options = vec![
        "Steer: hand it to the model as soon as it can take it".to_owned(),
        "Queue: send it once the answer ends".to_owned(),
    ];
    let picked = async {
        Menu::open("Send this message how?".to_owned(), options)?
            .answer()
            .await
    };
    match picked.await {
        Ok(Answer::Picked(index)) => Some(index == 0),
        _ => None,
    }
}

/// Lets the user reorder, edit and remove the messages waiting to be sent:
/// `texts` in delivery order, the first `steering` of them steering the
/// answer and the rest queued after it. Kinds belong to positions, so a
/// message moved across the boundary changes kind and its neighbour takes
/// the other. `None` when cancelled; else the messages and how many steer.
pub async fn manage(texts: Vec<String>, steering: usize) -> Option<(Vec<String>, usize)> {
    let mut menu = Manager {
        texts,
        steering,
        selected: 0,
        editing: false,
        rows: 0,
    };
    terminal::enable_raw_mode().ok()?;
    menu.run().await.ok().flatten()
}

struct Manager {
    texts: Vec<String>,
    steering: usize,
    selected: usize,
    editing: bool,
    rows: u16,
}

impl Manager {
    async fn run(&mut self) -> io::Result<Option<(Vec<String>, usize)>> {
        let mut input = EventStream::new();
        loop {
            if self.texts.is_empty() {
                return Ok(Some((Vec::new(), 0)));
            }
            self.draw()?;
            let Some(input) = input.next().await else {
                return Ok(None);
            };
            let Input::Key(key) = input? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let moving = key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
            if control && key.code == KeyCode::Char('c') {
                return Ok(None);
            }
            let last = self.texts.len() - 1;
            if self.editing {
                let text = &mut self.texts[self.selected];
                match key.code {
                    KeyCode::Enter if moving => text.push('\n'),
                    KeyCode::Enter | KeyCode::Esc => {
                        if text.trim().is_empty() {
                            self.remove();
                        }
                        self.editing = false;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if !control => text.push(c),
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Up if moving => self.shift(false),
                KeyCode::Down if moving => self.shift(true),
                KeyCode::Char('K') => self.shift(false),
                KeyCode::Char('J') => self.shift(true),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = self.selected.checked_sub(1).unwrap_or(last);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = if self.selected == last { 0 } else { self.selected + 1 };
                }
                KeyCode::Enter | KeyCode::Char('e') => self.editing = true,
                KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d' | 'x') => self.remove(),
                KeyCode::Esc | KeyCode::Char('q') => {
                    return Ok(Some((std::mem::take(&mut self.texts), self.steering)));
                }
                _ => {}
            }
        }
    }

    /// Moves the selected message a place down or up; the kinds stay with
    /// the places.
    fn shift(&mut self, down: bool) {
        let to = match down {
            true if self.selected + 1 < self.texts.len() => self.selected + 1,
            false if self.selected > 0 => self.selected - 1,
            _ => return,
        };
        self.texts.swap(self.selected, to);
        self.selected = to;
    }

    fn remove(&mut self) {
        self.texts.remove(self.selected);
        if self.selected < self.steering {
            self.steering -= 1;
        }
        self.selected = self.selected.min(self.texts.len().saturating_sub(1));
    }

    fn draw(&mut self) -> io::Result<()> {
        let mut out = stdout();
        queue!(out, MoveToColumn(0))?;
        if self.rows > 1 {
            queue!(out, MoveUp(self.rows - 1))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        let palette = palette();
        let width = render::terminal_width().saturating_sub(14).max(1);
        let mut lines = vec![format!(
            "{} {}",
            palette.accent.paint("?"),
            "Waiting messages".bold()
        )];
        for (index, text) in self.texts.iter().enumerate() {
            let label = match index < self.steering {
                true => "↳ steering",
                false => "+ queued  ",
            };
            let selected = index == self.selected;
            let mark = match selected {
                true => palette.accent.paint("❯").to_string(),
                false => " ".to_owned(),
            };
            let shown = match (selected, self.editing) {
                (true, true) => format!("{text}▏"),
                _ => text.clone(),
            };
            let rows = match selected {
                true => render::wrapped(&shown, width),
                false => vec![render::clip(&shown, width)],
            };
            for (n, row) in rows.iter().enumerate() {
                let head = if n == 0 { label } else { "          " };
                let row = match selected {
                    true => palette.name.paint(row).to_string(),
                    false => row.clone(),
                };
                let head = palette.muted.paint(head);
                lines.push(format!("{} {head} {row}", if n == 0 { &mark } else { " " }));
            }
        }
        let keys = match self.editing {
            true => "type to edit · Enter done · Alt+Enter newline · Esc done",
            false => "↑↓ select · Shift+↑↓ or J/K move · Enter edit · d remove · Esc apply · Ctrl+C cancel",
        };
        lines.push(palette.muted.paint(keys).to_string());
        queue!(out, Print(lines.join("\r\n")))?;
        self.rows = lines.len() as u16;
        out.flush()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let mut out = stdout();
        queue!(out, MoveToColumn(0)).ok();
        if self.rows > 1 {
            queue!(out, MoveUp(self.rows - 1)).ok();
        }
        queue!(out, Clear(ClearType::FromCursorDown)).ok();
        out.flush().ok();
        _ = terminal::disable_raw_mode();
    }
}

/// Asks the agent's `question`, offering `choices` and a typed answer.
async fn answer(question: &str, choices: &[String]) -> Decision {
    const TYPE: &str = "Type an answer…";
    let options: Vec<String> = choices.iter().cloned().chain([TYPE.to_owned()]).collect();
    let picked = async {
        let mut menu = Menu::open(question.to_owned(), options)?;
        match menu.answer().await? {
            Answer::Picked(index) if index < choices.len() => Ok(Some(choices[index].clone())),
            Answer::Picked(_) => menu.typed().await,
            Answer::Dismissed => Ok(None),
            Answer::Interrupted => Err(io::Error::from(ErrorKind::Interrupted)),
        }
    };
    match picked.await {
        Ok(Some(answer)) => {
            println!("  {} {answer}", palette().accent.paint("›"));
            Decision::Answer(answer)
        }
        Ok(None) => Decision::Decline,
        Err(_) => Decision::Stop,
    }
}

/// Shows the agent's `plan` and asks whether to carry it out: yes, no, or
/// what to change.
async fn approve_plan(plan: &str, style: &render::Style) -> Decision {
    println!();
    let mut reply = render::Reply::new(style);
    reply.text(plan);
    reply.finish();
    let options = [
        "Yes, carry it out".to_owned(),
        "No, keep planning".to_owned(),
        "Tell it what to change…".to_owned(),
    ];
    let picked = async {
        let mut menu = Menu::open(
            "Approve this plan and leave plan mode?".to_owned(),
            options.into(),
        )?;
        match menu.answer().await? {
            Answer::Picked(0) => io::Result::Ok(Decision::Once),
            Answer::Picked(1) | Answer::Dismissed => Ok(Decision::Decline),
            Answer::Picked(_) => Ok(menu
                .typed()
                .await?
                .map_or(Decision::Decline, Decision::Answer)),
            Answer::Interrupted => Ok(Decision::Stop),
        }
    };
    picked.await.unwrap_or(Decision::Stop)
}

enum Answer {
    Picked(usize),
    Dismissed,
    Interrupted,
}

/// A question and its options, drawn below the cursor in raw mode until it
/// is dropped, which clears it and leaves raw mode.
struct Menu {
    question: String,
    options: Vec<String>,
    selected: usize,
    /// Rows on screen, to clear before drawing again.
    rows: u16,
}

impl Menu {
    fn open(question: String, options: Vec<String>) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            question,
            options,
            selected: 0,
            rows: 0,
        })
    }

    async fn answer(&mut self) -> io::Result<Answer> {
        let mut input = EventStream::new();
        let last = self.options.len().saturating_sub(1);
        loop {
            self.draw()?;
            let Some(input) = input.next().await else {
                return Ok(Answer::Interrupted);
            };
            let Input::Key(key) = input? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(Answer::Interrupted);
                }
                KeyCode::Enter => return Ok(Answer::Picked(self.selected)),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(Answer::Dismissed),
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                    self.selected = self.selected.checked_sub(1).unwrap_or(last);
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                    self.selected = if self.selected == last {
                        0
                    } else {
                        self.selected + 1
                    };
                }
                _ => {}
            }
        }
    }

    /// Reads a line typed in place of the options; `None` when Esc
    /// dismisses it or nothing is typed.
    async fn typed(&mut self) -> io::Result<Option<String>> {
        let mut input = EventStream::new();
        let mut typed = String::new();
        self.options = vec![String::new()];
        loop {
            self.options[0] = format!("{typed}▏");
            self.selected = 0;
            self.draw()?;
            let Some(input) = input.next().await else {
                return Err(ErrorKind::Interrupted.into());
            };
            let Input::Key(key) = input? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(ErrorKind::Interrupted.into());
                }
                KeyCode::Enter => {
                    return Ok(Some(typed.trim().to_owned()).filter(|typed| !typed.is_empty()));
                }
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char(c) => typed.push(c),
                _ => {}
            }
        }
    }

    /// Draws in the colours dialoguer's prompts use, wrapping the question
    /// and options at the terminal's width so all of them can be read; the
    /// rows are counted from the wrapping so they can be erased.
    fn draw(&mut self) -> io::Result<()> {
        let mut out = stdout();
        self.erase(&mut out)?;
        let palette = palette();
        let width = render::terminal_width().saturating_sub(4).max(1);
        let mut lines = Vec::new();
        for (n, row) in render::wrapped(&self.question, width).iter().enumerate() {
            let mark = if n == 0 {
                palette.accent.paint("?").to_string()
            } else {
                " ".to_owned()
            };
            lines.push(format!("{mark} {}", row.bold()));
        }
        if let Some(last) = lines.last_mut() {
            last.push_str(&format!(" {}", palette.muted.paint("›")));
        }
        for (index, option) in self.options.iter().enumerate() {
            for (n, row) in render::wrapped(option, width).iter().enumerate() {
                lines.push(match (index == self.selected, n == 0) {
                    (true, true) => {
                        format!("{} {}", palette.accent.paint("❯"), palette.name.paint(row))
                    }
                    (true, false) => format!("  {}", palette.name.paint(row)),
                    (false, _) => format!("  {row}"),
                });
            }
        }
        queue!(out, Print(render::linkify(&lines.join("\r\n"))))?;
        self.rows = lines.len() as u16;
        out.flush()
    }

    /// Moves to the menu's first row and clears it and everything below.
    fn erase(&self, out: &mut impl Write) -> io::Result<()> {
        queue!(out, MoveToColumn(0))?;
        // `MoveUp(0)` would still move a row.
        if self.rows > 1 {
            queue!(out, MoveUp(self.rows - 1))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        let mut out = stdout();
        _ = self.erase(&mut out).and_then(|()| out.flush());
        _ = terminal::disable_raw_mode();
    }
}

/// Asks for a secret without showing it; `None` when nothing is entered.
/// `why_needed` explains the failure when there is no terminal to ask in.
pub fn secret(prompt: &str, why_needed: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        bail!("{why_needed}");
    }
    let secret = Password::with_theme(&theme())
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()?;
    Ok(Some(secret.trim().to_owned()).filter(|secret| !secret.is_empty()))
}

/// Picks one of `items`, starting at `current`; `None` when cancelled with
/// Esc or Ctrl+C.
pub fn pick(prompt: &str, items: &[String], current: usize) -> Result<Option<usize>> {
    // On Ctrl+C dialoguer raises SIGINT, which ends hey unless something
    // listens for it; the error that follows then reports the cancel.
    #[cfg(unix)]
    let _listening = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    println!();
    let picked = FuzzySelect::with_theme(&theme())
        .with_prompt(prompt)
        .items(items)
        .default(current)
        .max_length(15)
        .interact_opt();
    println!();
    match picked {
        Err(dialoguer::Error::IO(error)) if error.kind() == ErrorKind::Interrupted => Ok(None),
        picked => Ok(picked?),
    }
}

/// The alternate screen in raw mode, left however its user ends.
pub struct Screen;

impl Screen {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let screen = Self;
        execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        _ = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        _ = terminal::disable_raw_mode();
    }
}
