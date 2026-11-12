/* -------------------------------------------------------------------------- */
/*                                 sessions.rs                                */
/* -------------------------------------------------------------------------- */

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use rig_core::message::{Message, UserContent};
use rig_memory::{DirStore, Store};

const ID_FORMAT: &str = "%Y%m%d-%H%M%S%3f";

/// A conversation's log name: when it started, so names sort by age.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(Local::now().format(ID_FORMAT).to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn started(&self) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(&self.0, ID_FORMAT).ok()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.started() {
            Some(started) => write!(f, "{}", started.format("%Y-%m-%d %H:%M")),
            None => f.write_str(&self.0),
        }
    }
}

pub struct Summary {
    pub id: SessionId,
    pub title: String,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}  {}", self.id, self.title)
    }
}

/// Saved conversations: the agent appends to a session's log after every
/// run, so nothing here writes.
#[derive(Clone)]
pub struct Sessions {
    store: DirStore,
    dir: PathBuf,
}

impl Sessions {
    pub fn open(dir: PathBuf) -> Result<Self> {
        let store = DirStore::open(&dir)
            .with_context(|| format!("cannot open sessions in {}", dir.display()))?;
        Ok(Self { store, dir })
    }

    pub fn store(&self) -> DirStore {
        self.store.clone()
    }

    pub fn messages(&self, id: &SessionId) -> Result<Vec<Message>> {
        let conversation = rig_memory::load(&self.store, id.as_str())
            .with_context(|| format!("cannot read session {id}"))?;
        Ok(conversation
            .messages()
            .iter()
            .map(|message| Message::clone(message))
            .collect())
    }

    /// Every session with at least one message, newest first.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut ids: Vec<SessionId> = self
            .store
            .logs()
            .with_context(|| format!("cannot list sessions in {}", self.dir.display()))?
            .into_iter()
            .map(SessionId)
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let title = title(&self.messages(&id).ok()?)?;
                Some(Summary { id, title })
            })
            .collect())
    }

    pub fn latest(&self) -> Result<Option<SessionId>> {
        Ok(self.list()?.into_iter().next().map(|summary| summary.id))
    }
}

/// The start of the first thing the user typed.
fn title(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::User { content } => content.iter().find_map(|content| match content {
            UserContent::Text(text) => Some(rig_memory::title_of(&text.text)),
            _ => None,
        }),
        _ => None,
    })
}
