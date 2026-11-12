/* -------------------------------------------------------------------------- */
/*                                 sessions.rs                                */
/* -------------------------------------------------------------------------- */

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use rig_core::message::{Message, UserContent};
use rig_memory::{DirStore, Store};
use serde::{Deserialize, Serialize};

use crate::mention;

const ID_FORMAT: &str = "%Y%m%d-%H%M%S%3f";

/// A conversation's log name: when it started, so names sort by age. Names
/// arrive from clients, so they hold only characters safe in a file name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
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

impl FromStr for SessionId {
    type Err = String;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        let valid = (1..=64).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        valid
            .then(|| Self(id.to_owned()))
            .ok_or_else(|| format!("invalid thread id {id:?}"))
    }
}

impl TryFrom<String> for SessionId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        id.parse()
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            .filter_map(|log| log.parse().ok())
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

    /// The saved session `id`, if there is one.
    pub fn find(&self, id: &str) -> Option<SessionId> {
        let id: SessionId = id.parse().ok()?;
        let logs = self.store.logs().ok()?;
        logs.iter().any(|log| log == id.as_str()).then_some(id)
    }
}

/// The start of the first thing the user typed.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_clients_cannot_name_other_files() {
        assert!(SessionId::new().as_str().parse::<SessionId>().is_ok());
        for id in ["", "..", "../x", "a/b", "a\\b", "a b", &"x".repeat(65)] {
            assert!(id.parse::<SessionId>().is_err(), "{id:?}");
        }
        assert!(serde_json::from_str::<SessionId>("\"../x\"").is_err());
    }
}

/// The start of the first thing the user typed.
fn title(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::User { content } => content.iter().find_map(|content| match content {
            UserContent::Text(text) if !mention::is_context(&text.text) => {
                Some(rig_memory::title_of(&text.text))
            }
            _ => None,
        }),
        _ => None,
    })
}
