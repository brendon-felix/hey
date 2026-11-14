/* -------------------------------------------------------------------------- */
/*                              render/screen.rs                              */
/* -------------------------------------------------------------------------- */

//! Where output goes. While nothing reads input, it goes straight to the
//! terminal. While the line editor reads, lines go above its prompt and the
//! rows redrawn as an answer goes on (its open row, a preview, the spinner)
//! show at the top of the prompt, so typing and the answer share the screen.

use std::fmt::Display;
use std::io::{self, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crossterm::Command;
use crossterm::cursor::MoveUp;
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use reedline::{ExternalOutput, RepaintSignal};

static SCREEN: Mutex<Screen> = Mutex::new(Screen {
    mode: Mode::Direct { drawn: 0 },
    live: Vec::new(),
});

struct Screen {
    mode: Mode,
    /// The rows to redraw, each narrower than the terminal.
    live: Vec<String>,
}

enum Mode {
    /// Printed as it comes, with this many live rows drawn below it.
    Direct { drawn: usize },
    /// The line editor reads, or will again with its prompt where it was:
    /// it prints `lines` above its prompt as it drains them, and draws the
    /// live rows in the prompt.
    Editing {
        lines: Vec<String>,
        repaint: RepaintSignal,
        /// Set once what waits is printed, to break the read off.
        wake: Option<Arc<AtomicBool>>,
    },
}

/// Prints `text` and ends its line. Live rows give way until next set.
pub fn line(text: impl Display) {
    let text = text.to_string();
    let mut screen = lock();
    let Screen { mode, live } = &mut *screen;
    match mode {
        Mode::Direct { drawn } => {
            let erase = erase(std::mem::take(drawn));
            show(format!("{erase}{text}\n"));
        }
        Mode::Editing { lines, repaint, .. } => {
            lines.push(text);
            live.clear();
            repaint.request_repaint();
        }
    }
}

/// Redraws the live rows as `rows`.
pub fn set_live(rows: Vec<String>) {
    let mut screen = lock();
    let unchanged = screen.live == rows;
    match &mut screen.mode {
        Mode::Direct { drawn } if unchanged && (*drawn > 0 || rows.is_empty()) => {}
        Mode::Direct { drawn } => {
            let erase = erase(std::mem::replace(drawn, rows.len()));
            show(format!(
                "{}{erase}{}{}",
                ansi(BeginSynchronizedUpdate),
                rows.join("\n"),
                ansi(EndSynchronizedUpdate)
            ));
        }
        Mode::Editing { .. } if unchanged => {}
        Mode::Editing { repaint, .. } => repaint.request_repaint(),
    }
    screen.live = rows;
}

/// The live rows, for the line editor's prompt to show.
pub fn live() -> Vec<String> {
    lock().live.clone()
}

/// Hands output to the line editor, which is about to read with `repaint`
/// redrawing its prompt. What it had yet to print still waits for it.
pub fn editing(repaint: RepaintSignal) {
    let mut screen = lock();
    match &mut screen.mode {
        Mode::Direct { drawn } => {
            show(erase(*drawn));
            screen.mode = Mode::Editing {
                lines: Vec::new(),
                repaint,
                wake: None,
            };
        }
        Mode::Editing { repaint: current, .. } => *current = repaint,
    }
}

/// Takes output back from the line editor, which stopped reading below its
/// prompt, printing what it had yet to.
pub fn direct() {
    let mut screen = lock();
    if let Mode::Editing { lines, .. } = std::mem::replace(&mut screen.mode, Mode::Direct { drawn: 0 }) {
        show(lines.iter().map(|line| format!("{line}\n")).collect::<String>());
    }
}

/// Sets `wake` once the line editor has printed what waits for it, so what
/// led up to the reason for waking shows first.
pub fn wake_when_printed(wake: Arc<AtomicBool>) {
    match &mut lock().mode {
        Mode::Editing { lines, wake: waiting, .. } if !lines.is_empty() => *waiting = Some(wake),
        Mode::Editing { .. } | Mode::Direct { .. } => wake.store(true, Ordering::Relaxed),
    }
}

/// What the line editor prints above its prompt.
pub struct Feed;

impl ExternalOutput for Feed {
    fn drain(&mut self) -> io::Result<Vec<String>> {
        Ok(match &mut lock().mode {
            Mode::Editing { lines, wake, .. } => {
                // Taken now, set once these print: reedline looks for a
                // break before it prints, so the wake breaks the next read.
                if let Some(wake) = wake.take() {
                    wake.store(true, Ordering::Relaxed);
                }
                std::mem::take(lines)
            }
            Mode::Direct { .. } => Vec::new(),
        })
    }
}

/// What returns the cursor to the first of `rows` drawn below the output,
/// clearing them.
fn erase(rows: usize) -> String {
    match rows {
        0 => String::new(),
        1 => format!("\r{}", Clear(ClearType::FromCursorDown)),
        rows => format!("\r{}{}", MoveUp(rows as u16 - 1), Clear(ClearType::FromCursorDown)),
    }
}

fn show(text: String) {
    if !text.is_empty() {
        let mut out = stdout();
        _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    }
}

fn ansi(command: impl Command) -> String {
    let mut out = String::new();
    let _ = command.write_ansi(&mut out);
    out
}

fn lock() -> MutexGuard<'static, Screen> {
    SCREEN.lock().unwrap_or_else(PoisonError::into_inner)
}
