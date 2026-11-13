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
pub async fn confirm(ask: &Ask) -> Decision {
    if !std::io::stdin().is_terminal() {
        return Decision::Stop;
    }
    let decisions = [Decision::Once, Decision::Always, Decision::Decline];
    let (question, labels) = match ask {
        Ask::Tool {
            name,
            agent,
            machine,
        } => {
            let on = machine.as_ref().map(|machine| format!(" on {machine}")).unwrap_or_default();
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
        Ok(Answer::Picked(index)) => decisions[index],
        Ok(Answer::Dismissed) => Decision::Decline,
        Ok(Answer::Interrupted) | Err(_) => Decision::Stop,
    }
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
                    self.selected = if self.selected == last { 0 } else { self.selected + 1 };
                }
                _ => {}
            }
        }
    }

    /// Draws in the colours dialoguer's prompts use, each row clipped so
    /// none wraps and the rows stay countable.
    fn draw(&mut self) -> io::Result<()> {
        let mut out = stdout();
        self.erase(&mut out)?;
        let palette = palette();
        let width = render::terminal_width().saturating_sub(4);
        let question = render::clip(&self.question, width);
        queue!(
            out,
            Print(format!(
                "{} {} {}",
                palette.accent.paint("?"),
                question.bold(),
                palette.muted.paint("›")
            ))
        )?;
        for (index, option) in self.options.iter().enumerate() {
            let option = render::clip(option, width);
            let row = match index == self.selected {
                true => format!("{} {}", palette.accent.paint("❯"), palette.name.paint(&option)),
                false => format!("  {option}"),
            };
            queue!(out, Print("\r\n"), Print(row))?;
        }
        self.rows = 1 + self.options.len() as u16;
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
