/* -------------------------------------------------------------------------- */
/*                                 sessions.rs                                */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use rig_core::conversation::Conversation;
use rig_core::message::{Message, UserContent};
use rig_memory::{DirStore, Store, SummaryState};
use serde::{Deserialize, Serialize};

use crate::{compaction, mention};

const ID_FORMAT: &str = "%Y%m%d-%H%M%S%3f";

/// Where the marks of a store's conversations are kept, beside its `logs/`;
/// the store reads only its own folders, so it leaves the file alone.
const MARKS: &str = "threads.json";

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

    /// When the conversation started, for an id hey made.
    pub fn started(&self) -> Option<NaiveDateTime> {
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

/// What a listing shows of a conversation beside its messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mark {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// An answer ended that no client has shown yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unseen: bool,
}

/// A conversation as a compaction of it builds on: its messages, and how
/// many of the first are a summary already, with that summary.
pub struct Basis {
    pub messages: Vec<Arc<Message>>,
    pub from: usize,
    pub state: SummaryState,
}

/// Saved conversations: the agent appends to a session's log after every
/// run; hey itself copies, removes, marks and compacts them.
#[derive(Clone)]
pub struct Sessions {
    store: DirStore,
    dir: PathBuf,
    /// Held while the marks file is read and rewritten.
    marking: Arc<Mutex<()>>,
}

impl Sessions {
    pub fn open(dir: PathBuf) -> Result<Self> {
        let store = DirStore::open(&dir)
            .with_context(|| format!("cannot open sessions in {}", dir.display()))?;
        Ok(Self {
            store,
            dir,
            marking: Arc::default(),
        })
    }

    pub fn store(&self) -> DirStore {
        self.store.clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
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

    /// Adds `messages` to the conversation `id`, provided it still holds the
    /// `held` messages it held when they were said; else nothing, returning
    /// false.
    pub fn append(&self, id: &SessionId, held: usize, messages: Vec<Message>) -> Result<bool> {
        let mut conversation = self.conversation(id)?;
        if conversation.messages().len() != held {
            return Ok(false);
        }
        conversation.extend(messages);
        self.save(id, &mut conversation)?;
        Ok(true)
    }

    /// The conversation `id` as a compaction of it builds on.
    pub fn basis(&self, id: &SessionId) -> Result<Basis> {
        let conversation = self.conversation(id)?;
        Ok(Basis {
            messages: conversation.messages().to_vec(),
            from: rig_memory::summarized_prefix(&conversation),
            state: rig_memory::summary_state(&conversation),
        })
    }

    /// Puts the summary of `state` in place of the messages of `id` before
    /// its `at`th, provided that is still `kept`; else nothing, returning
    /// false. What it replaces stays in the log.
    pub fn summarize(&self, id: &SessionId, at: usize, kept: &Message, state: &SummaryState) -> Result<bool> {
        let mut conversation = self.conversation(id)?;
        if conversation.messages().get(at).map(|message| &**message) != Some(kept) {
            return Ok(false);
        }
        let keep_from = conversation.id(at);
        if rig_memory::apply_summary(&mut conversation, keep_from, state).is_none() {
            return Ok(false);
        }
        self.save(id, &mut conversation)?;
        Ok(true)
    }

    fn conversation(&self, id: &SessionId) -> Result<Conversation> {
        rig_memory::load(&self.store, id.as_str()).with_context(|| format!("cannot read session {id}"))
    }

    fn save(&self, id: &SessionId, conversation: &mut Conversation) -> Result<()> {
        rig_memory::save(&self.store, id.as_str(), conversation).with_context(|| format!("cannot save session {id}"))
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
                let title = self.title(&id)?;
                Some(Summary { id, title })
            })
            .collect())
    }

    /// The start of the first thing the user typed in `id`, which a
    /// compaction may have replaced with a summary; its log keeps it.
    fn title(&self, id: &SessionId) -> Option<String> {
        let messages = self.messages(id).ok()?;
        let compacted = messages.first().is_some_and(is_summary);
        let first = compacted
            .then(|| rig_memory::summarized_in_log(&self.store, id.as_str()).ok())
            .flatten()
            .and_then(|summarized| {
                let replaced = summarized.replaced.into_values().flatten();
                replaced.min_by_key(|entry| entry.made).map(|entry| entry.message)
            });
        first.and_then(|message| title(std::slice::from_ref(&*message))).or_else(|| title(&messages))
    }

    /// The saved session `id`, if there is one.
    pub fn find(&self, id: &str) -> Option<SessionId> {
        let id: SessionId = id.parse().ok()?;
        let logs = self.store.logs().ok()?;
        logs.iter().any(|log| log == id.as_str()).then_some(id)
    }

    /// Starts a new conversation as a copy of what `id` saved, with a
    /// lineage of its own so the two go on apart.
    pub fn fork(&self, id: &SessionId) -> Result<SessionId> {
        let messages = self.messages(id)?;
        anyhow::ensure!(!messages.is_empty(), "nothing is saved in this conversation yet");
        let copy = SessionId::new();
        let mut conversation = Conversation::from_messages(messages);
        rig_memory::save(&self.store, copy.as_str(), &mut conversation)
            .with_context(|| format!("cannot save the copy of {id}"))?;
        Ok(copy)
    }

    /// Removes the conversation `id` and its mark; false when nothing was
    /// saved. Images it saved stay among the store's blobs, which other
    /// conversations may share.
    pub fn delete(&self, id: &SessionId) -> Result<bool> {
        // The store keeps a log as `logs/<name>.jsonl` and has no call to
        // remove one.
        let log = self.dir.join("logs").join(format!("{}.jsonl", id.as_str()));
        let removed = match fs::remove_file(&log) {
            Ok(()) => true,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error).with_context(|| format!("cannot remove {}", log.display())),
        };
        self.mark(id, |mark| *mark = Mark::default())?;
        Ok(removed)
    }

    /// Every conversation's mark; one without is plain.
    pub fn marks(&self) -> Result<BTreeMap<SessionId, Mark>> {
        let path = self.dir.join(MARKS);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("cannot read {}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Changes the mark of `id`. The file is read just before it is
    /// rewritten, so another hey keeping the same store loses little.
    pub fn mark(&self, id: &SessionId, change: impl FnOnce(&mut Mark)) -> Result<()> {
        let _marking = self.marking.lock().unwrap_or_else(PoisonError::into_inner);
        let mut marks = self.marks()?;
        let mark = marks.entry(id.clone()).or_default();
        let before = *mark;
        change(mark);
        if *mark == before {
            return Ok(());
        }
        if *mark == Mark::default() {
            marks.remove(id);
        }
        let path = self.dir.join(MARKS);
        let written = path.with_extension("json.new");
        fs::write(&written, serde_json::to_vec_pretty(&marks)?)
            .and_then(|()| fs::rename(&written, &path))
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// The start of the first thing the user typed.
fn title(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::User { content } => content.iter().find_map(|content| match content {
            UserContent::Text(text) if !mention::is_context(&text.text) && compaction::summary_in(&text.text).is_none() => {
                Some(rig_memory::title_of(&text.text))
            }
            _ => None,
        }),
        _ => None,
    })
}

fn is_summary(message: &Message) -> bool {
    let Message::User { content } = message else {
        return false;
    };
    content.iter().any(|content| match content {
        UserContent::Text(text) => compaction::summary_in(&text.text).is_some(),
        _ => false,
    })
}

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

    #[test]
    fn forks_go_on_apart_and_deletes_take_the_mark() {
        let dir = std::env::temp_dir().join(format!("hey-sessions-test-{}", std::process::id()));
        let sessions = Sessions::open(dir.clone()).unwrap();
        let original: SessionId = "20260101-000000000".parse().unwrap();
        let mut conversation = Conversation::from_messages([Message::user("first question")]);
        rig_memory::save(&sessions.store, original.as_str(), &mut conversation).unwrap();

        let copy = sessions.fork(&original).unwrap();
        let mut copied = rig_memory::load(&sessions.store, copy.as_str()).unwrap();
        copied.push(Message::user("only in the copy"));
        rig_memory::save(&sessions.store, copy.as_str(), &mut copied).unwrap();
        assert_eq!(sessions.messages(&original).unwrap().len(), 1);
        assert_eq!(sessions.messages(&copy).unwrap().len(), 2);

        sessions.mark(&original, |mark| mark.archived = true).unwrap();
        assert!(sessions.marks().unwrap()[&original].archived);
        assert!(sessions.delete(&original).unwrap());
        assert!(!sessions.marks().unwrap().contains_key(&original));
        assert!(sessions.messages(&original).unwrap().is_empty());
        assert!(!sessions.delete(&original).unwrap(), "already gone");
        assert_eq!(sessions.list().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_summary_replaces_older_messages_but_not_the_title() {
        let dir = std::env::temp_dir().join(format!("hey-compaction-test-{}", std::process::id()));
        let sessions = Sessions::open(dir.clone()).unwrap();
        let id: SessionId = "20260101-000000000".parse().unwrap();
        let said = ["first question", "first answer", "second question", "second answer"];
        let messages = said.iter().enumerate().map(|(at, text)| match at % 2 {
            0 => Message::user(*text),
            _ => Message::assistant(*text),
        });
        let mut conversation = Conversation::from_messages(messages);
        rig_memory::save(&sessions.store, id.as_str(), &mut conversation).unwrap();
        let state = SummaryState {
            summary: "Asked twice.".into(),
            ..SummaryState::default()
        };

        let stale = Message::user("not what is there");
        assert!(!sessions.summarize(&id, 2, &stale, &state).unwrap(), "a changed thread is left alone");
        assert!(sessions.summarize(&id, 2, &Message::user("second question"), &state).unwrap());

        let basis = sessions.basis(&id).unwrap();
        assert_eq!(basis.messages.len(), 3);
        assert_eq!((basis.from, basis.state.summary.as_str()), (1, "Asked twice."));
        assert_eq!(sessions.list().unwrap()[0].title, "first question");
        fs::remove_dir_all(dir).unwrap();
    }
}
