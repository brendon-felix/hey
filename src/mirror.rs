/* -------------------------------------------------------------------------- */
/*                                  mirror.rs                                 */
/* -------------------------------------------------------------------------- */

//! A store that keeps each conversation's Markdown beside its log, as Zed
//! keeps a thread you can open as a file. The log stays the truth, with the
//! tool calls and reasoning Markdown leaves out; every write to it, from the
//! agent or from hey, redraws the file, so an editor, `bat` or Obsidian
//! pointed at it sees the conversation as it goes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rig_core::message::Message;
use rig_memory::{DirStore, Files, LogWriteError, Store};

use crate::sessions::SessionId;
use crate::transcript;

#[derive(Clone)]
pub struct Mirror {
    inner: DirStore,
    /// Where the Markdown files go; none writes no files.
    dir: Option<PathBuf>,
}

impl Mirror {
    pub fn new(inner: DirStore, dir: Option<PathBuf>) -> Self {
        Self { inner, dir }
    }

    /// Where the Markdown of conversation `id` is, when files are kept.
    pub fn path(&self, id: &SessionId) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{}.md", id.as_str())))
    }

    /// Draws the file of the log `name` anew from the log, as a write to it
    /// does. A failure is logged and goes no further: the log is saved.
    pub fn refresh(&self, name: &str) {
        let Ok(id) = name.parse::<SessionId>() else {
            return;
        };
        if let Err(error) = self.draw(&id) {
            tracing::warn!(
                thread = name,
                error = format!("{error:#}"),
                "cannot write the Markdown of a thread"
            );
        }
    }

    fn draw(&self, id: &SessionId) -> anyhow::Result<()> {
        let Some(path) = self.path(id) else {
            return Ok(());
        };
        let conversation = rig_memory::load(&self.inner, id.as_str())?;
        let messages: Vec<Message> = conversation
            .messages()
            .iter()
            .map(|message| Message::clone(message))
            .collect();
        let started = id
            .started()
            .map(|started| started.format("%Y-%m-%dT%H:%M:%S").to_string());
        match transcript::document(id.as_str(), started.as_deref(), &messages) {
            Some(text) => write_atomically(&path, &text)?,
            None => remove(&path)?,
        }
        Ok(())
    }

    /// Removes the file of conversation `id`.
    pub fn forget(&self, id: &SessionId) -> io::Result<()> {
        match self.path(id) {
            Some(path) => remove(&path),
            None => Ok(()),
        }
    }
}

/// A reader of the file sees the old text or the new, and an editor
/// watching it reloads once.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let written = path.with_extension("md.tmp");
    fs::write(&written, text)?;
    fs::rename(&written, path)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

impl Files for Mirror {
    fn write_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_file(name, bytes)
    }

    fn append_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.inner.append_file(name, bytes)
    }

    fn read_file(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read_file(name)
    }

    fn remove_file(&self, name: &str) -> io::Result<()> {
        self.inner.remove_file(name)
    }

    fn files(&self) -> io::Result<Vec<String>> {
        self.inner.files()
    }
}

impl Store for Mirror {
    fn logs(&self) -> io::Result<Vec<String>> {
        self.inner.logs()
    }

    fn read_log(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read_log(name)
    }

    fn append_log(&self, name: &str, len: u64, bytes: &[u8]) -> Result<(), LogWriteError> {
        self.inner.append_log(name, len, bytes)?;
        self.refresh(name);
        Ok(())
    }

    fn replace_log(&self, name: &str, len: u64, bytes: &[u8]) -> Result<(), LogWriteError> {
        self.inner.replace_log(name, len, bytes)?;
        self.refresh(name);
        Ok(())
    }

    fn destroy(&self) -> io::Result<()> {
        self.inner.destroy()
    }
}

#[cfg(test)]
mod tests {
    use rig_core::conversation::Conversation;

    use super::*;

    #[test]
    fn every_write_to_a_log_redraws_its_markdown() {
        let root = std::env::temp_dir().join(format!("hey-mirror-test-{}", std::process::id()));
        let mirror = Mirror::new(
            DirStore::open(root.join("store")).unwrap(),
            Some(root.join("md")),
        );
        let id: SessionId = "20260101-000000000".parse().unwrap();
        let path = mirror.path(&id).unwrap();

        let mut conversation = Conversation::from_messages([Message::user("first question")]);
        rig_memory::save(&mirror, id.as_str(), &mut conversation).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\ntitle: \"first question\""), "{text}");
        assert!(text.contains("## User\n\nfirst question"));

        conversation.push(Message::assistant("an answer"));
        rig_memory::save(&mirror, id.as_str(), &mut conversation).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("## Assistant\n\nan answer"));

        mirror.forget(&id).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
