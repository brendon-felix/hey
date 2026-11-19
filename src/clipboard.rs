/* -------------------------------------------------------------------------- */
/*                                clipboard.rs                                */
/* -------------------------------------------------------------------------- */

use std::io::stdout;

use anyhow::Result;
use crossterm::clipboard::CopyToClipboard;
use crossterm::execute;

/// Where copied text went.
pub enum Copied {
    /// This machine's clipboard.
    System,
    /// The terminal, as OSC 52, which sets the clipboard of the machine it
    /// runs on if it supports that. Over SSH that is the one the user sits
    /// at; otherwise this machine has no clipboard hey can reach, and why.
    Terminal(Option<arboard::Error>),
}

/// The clipboard of the machine the user sits at.
#[derive(Default)]
pub struct Clipboard {
    /// Kept for the session, since on Linux what is copied stays on the
    /// clipboard only while the clipboard that copied it lives.
    system: Option<arboard::Clipboard>,
}

impl Clipboard {
    pub fn copy(&mut self, text: &str) -> Result<Copied> {
        let unavailable = if over_ssh() {
            None
        } else {
            match self.system(text) {
                Ok(()) => return Ok(Copied::System),
                Err(error) => Some(error),
            }
        };
        execute!(stdout(), CopyToClipboard::to_clipboard_from(text))?;
        Ok(Copied::Terminal(unavailable))
    }

    fn system(&mut self, text: &str) -> Result<(), arboard::Error> {
        let system = match &mut self.system {
            Some(system) => system,
            None => self.system.insert(arboard::Clipboard::new()?),
        };
        system.set_text(text)
    }
}

/// Whether this machine is one the user reached over SSH, whose clipboard
/// is not theirs.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|variable| std::env::var_os(variable).is_some())
}

/// Saves the clipboard's image in hey's cache and returns the mention that
/// attaches it, `@"path"`; `None` when the clipboard holds no image.
pub fn paste_image() -> Result<Option<String>> {
    let dir = crate::config::cache_dir()
        .map(|dir| dir.join("pasted"))
        .ok_or_else(|| anyhow::anyhow!("no cache directory on this platform"))?;
    let launcher = rig_tools::process::Launcher::default();
    let saved = rig_tools::clipboard::read_image(&launcher, &dir)?;
    Ok(saved.map(|path| crate::mention::typed(&path.display().to_string())))
}
