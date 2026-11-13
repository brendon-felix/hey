/* -------------------------------------------------------------------------- */
/*                               memory/mod.rs                                */
/* -------------------------------------------------------------------------- */

mod vectors;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rig_agent::extractor::ExtractorBuilder;
use rig_core::message::{AssistantContent, Message, UserContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::{self, Model};
use crate::config::{self, Providers};
use vectors::Vectors;

const HEADER: &str = "# Memories\n\n\
What hey remembers across conversations, oldest first. Edit freely: hey keeps \
the lines that start with `- ` and rewrites the file as it learns.\n\n";

const LEARNING_PROMPT: &str = "\
You keep the long-term memory of hey, a terminal assistant: the few things it should know about \
its user in later, unrelated conversations. Read the exchange, then submit:
- `remember`: facts about the user that will still hold months from now and help in many \
conversations, not just ones like this: who they are, their background and work, lasting \
preferences and how they like to be answered, the machines they use and what each is for, and \
the ongoing projects they work on, what each is and where it lives. Write each as one short, \
self-contained sentence about the user.
- `forget`: the numbers of saved memories the exchange shows to be wrong or outdated. To correct \
a memory, forget it and remember the corrected version.
Do not remember:
- hey itself: its configuration, models, tools, MCP servers, limits or behavior.
- The state of a machine, directory, repository or codebase: what is installed, configured, \
checked out or available, what a directory holds, how code is structured or where things are \
defined. These change, and the assistant can look them up when it needs them.
- The task at hand: its details, progress, findings, bugs and fixes.
- What is already saved, general knowledge, the assistant's suggestions the user did not adopt, \
or secrets such as passwords, keys and tokens.
When unsure whether a fact is lasting and broadly useful, leave it out. Most exchanges teach \
nothing lasting: then submit two empty lists.";

/// Most saved memories the model sees when deciding what an exchange changes.
const LEARNING_VIEW: usize = 50;

/// Longest a message is quoted to the model learning from it.
const QUOTED: usize = 4000;

/// Longest a conversation waits to start for the embedding model, which may
/// be on another machine or still loading.
const RECALL_WAIT: Duration = Duration::from_secs(20);

/// Lasting facts about the user and their work, learned from answers and
/// recalled by meaning as conversations start. Kept as a Markdown list that
/// a person can read and edit, read afresh on every use so their edits count.
pub struct Memory {
    file: PathBuf,
    vectors: Vectors,
    recall: usize,
    model: Option<String>,
    /// Held across each read, change and write, so updates in this process
    /// do not undo each other.
    writing: Mutex<()>,
}

/// The memories a conversation starts with.
pub struct Recalled {
    pub memories: Vec<String>,
    /// Why they are the newest rather than the closest in meaning.
    pub fallback: Option<anyhow::Error>,
}

impl Memory {
    /// `None` when memory is off.
    pub fn open(config: &config::Memory, providers: &Providers) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let file = config.file()?;
        let embedder = agent::embedding_model(&config.embedding_model, providers)?;
        Ok(Some(Self {
            vectors: Vectors::new(
                &config.embedding_model,
                embedder,
                file.with_extension("vectors.json"),
            ),
            file,
            recall: config.recall,
            model: config.model.clone(),
            writing: Mutex::default(),
        }))
    }

    /// Where memories are kept.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Every memory, oldest first.
    pub fn all(&self) -> Result<Vec<String>> {
        match fs::read_to_string(&self.file) {
            Ok(text) => Ok(parse(&text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error)
                .with_context(|| format!("cannot read memories {}", self.file.display())),
        }
    }

    /// What a conversation that opens with `prompt` starts out knowing: the
    /// memories closest to it in meaning, or the newest when the embedding
    /// model cannot say which those are.
    pub async fn recall(&self, prompt: &str) -> Result<Recalled> {
        let memories = self.all()?;
        let closest = tokio::time::timeout(
            RECALL_WAIT,
            self.vectors.closest(&memories, prompt, self.recall),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow!("the embedding model took too long")));
        Ok(match closest {
            Ok(closest) => Recalled {
                memories: closest.into_iter().cloned().collect(),
                fallback: None,
            },
            Err(error) => Recalled {
                memories: newest(&memories, self.recall).to_vec(),
                fallback: Some(error),
            },
        })
    }

    /// False when it, or one differing only in case, is already saved.
    pub fn remember(&self, memory: &str) -> Result<bool> {
        let memory = written(memory)?;
        self.change(|memories| add(memories, memory))
    }

    /// Rewords `old` as `new` where it stands, or saves `new` when `old` is
    /// gone, as when hey forgot it meanwhile.
    pub fn revise(&self, old: &str, new: &str) -> Result<()> {
        let new = written(new)?;
        self.change(|memories| reword(memories, old, new))?;
        Ok(())
    }

    /// False when `memory` was not saved.
    pub fn forget(&self, memory: &str) -> Result<bool> {
        self.change(|memories| {
            let count = memories.len();
            memories.retain(|saved| saved != memory);
            memories.len() != count
        })
    }

    /// The model configured to decide what to remember, rather than the
    /// conversation's.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Has `model` read `exchange`, alongside the saved memories closest to
    /// it, then saves what it teaches about the user and forgets what it
    /// shows to be wrong.
    pub async fn learn(&self, model: &Model, exchange: &[Message]) -> Result<()> {
        let transcript = transcript(exchange);
        if transcript.is_empty() {
            return Ok(());
        }
        let saved = self.all()?;
        let shown = self
            .vectors
            .closest(&saved, &transcript, LEARNING_VIEW)
            .await?;
        let update = ExtractorBuilder::<Update>::new(model.completion())
            .append_preamble(LEARNING_PROMPT)
            .retries(1)
            .build()
            .extract(learning_request(&shown, &transcript))
            .await
            .context("cannot learn from the answer")?
            .output;
        self.change(|memories| update.apply(&shown, memories))?;
        Ok(())
    }

    /// Applies `edit` to the saved memories, writing them back unless it
    /// returns false for no change.
    fn change(&self, edit: impl FnOnce(&mut Vec<String>) -> bool) -> Result<bool> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut memories = self.all()?;
        if !edit(&mut memories) {
            return Ok(false);
        }
        let text: String = std::iter::once(HEADER.to_owned())
            .chain(memories.iter().map(|memory| format!("- {memory}\n")))
            .collect();
        replace(&self.file, &text)
            .with_context(|| format!("cannot save memories {}", self.file.display()))?;
        Ok(true)
    }
}

/// The preamble section that tells a conversation what it recalled.
pub fn preamble(memories: &[String]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let list: String = memories.iter().map(|memory| format!("\n- {memory}")).collect();
    Some(format!(
        "\nMemories from earlier conversations, which may be outdated; what the user says now \
         takes precedence:{list}"
    ))
}

/// What an exchange changes in memory.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct Update {
    /// New lasting, broadly useful facts about the user, each one short,
    /// self-contained sentence.
    remember: Vec<String>,
    /// Numbers of saved memories the exchange shows to be wrong or outdated.
    forget: Vec<u32>,
}

impl Update {
    /// Forgets the memories of `shown` that `forget` numbers from 1, then
    /// adds the new ones. False when nothing changed.
    fn apply(self, shown: &[&String], memories: &mut Vec<String>) -> bool {
        let forgotten: Vec<&String> = self
            .forget
            .iter()
            .filter_map(|&number| shown.get(usize::try_from(number).ok()?.checked_sub(1)?))
            .copied()
            .collect();
        let count = memories.len();
        memories.retain(|memory| !forgotten.contains(&memory));
        let mut changed = memories.len() != count;
        for memory in &self.remember {
            changed |= add(memories, one_line(memory));
        }
        changed
    }
}

/// Saves `memory` unless it is empty or already saved, ignoring case.
fn add(memories: &mut Vec<String>, memory: String) -> bool {
    let known = memories.iter().any(|saved| saved.eq_ignore_ascii_case(&memory));
    let added = !memory.is_empty() && !known;
    if added {
        memories.push(memory);
    }
    added
}

/// Rewords `old` as `new`, or adds `new` when `old` is gone, leaving one
/// memory that says `new` either way. False when nothing changed.
fn reword(memories: &mut Vec<String>, old: &str, new: String) -> bool {
    let at = memories.iter().position(|saved| saved == old);
    let known = memories
        .iter()
        .enumerate()
        .any(|(i, saved)| Some(i) != at && saved.eq_ignore_ascii_case(&new));
    match (at, known) {
        (Some(at), false) => memories[at] = new,
        (Some(at), true) => _ = memories.remove(at),
        (None, false) => memories.push(new),
        (None, true) => return false,
    }
    true
}

/// `text` as a memory a person wrote, which must say something.
fn written(text: &str) -> Result<String> {
    match one_line(text) {
        memory if memory.is_empty() => bail!("a memory cannot be empty"),
        memory => Ok(memory),
    }
}

fn learning_request(shown: &[&String], transcript: &str) -> String {
    let saved = match shown {
        [] => "(none)".to_owned(),
        shown => shown
            .iter()
            .zip(1..)
            .map(|(memory, number)| format!("{number}. {memory}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    format!("Saved memories:\n{saved}\n\nExchange:\n{transcript}")
}

/// What was said in `exchange`, leaving out tool calls and their output.
fn transcript(exchange: &[Message]) -> String {
    let said = |speaker: &str, text: &str| format!("{speaker}: {}", clip(text, QUOTED));
    exchange
        .iter()
        .flat_map(|message| match message {
            Message::System { .. } => Vec::new(),
            Message::User { content } => content
                .iter()
                .filter_map(|content| match content {
                    UserContent::Text(text) => Some(said("User", &text.text)),
                    _ => None,
                })
                .collect(),
            Message::Assistant(reply) => reply
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(text) => Some(said("Assistant", &text.text)),
                    _ => None,
                })
                .collect(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn clip(text: &str, length: usize) -> String {
    match text.char_indices().nth(length) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn newest(memories: &[String], limit: usize) -> &[String] {
    &memories[memories.len().saturating_sub(limit)..]
}

fn parse(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("- "))
        .map(one_line)
        .filter(|memory| !memory.is_empty())
        .collect()
}

/// `text` as a memory is saved: on one line, its spacing collapsed.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replaces `file` whole, so a reader never sees half of it.
fn replace(file: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut temporary = OsString::from(file);
    temporary.push(".tmp");
    fs::write(&temporary, contents)?;
    fs::rename(&temporary, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    #[test]
    fn updates_forget_by_shown_number_and_skip_known_memories() {
        let mut memories = parse(&format!(
            "{HEADER}- The user lives in Lisbon.\n- The user  uses\tVim.\nnot a memory\n"
        ));
        assert_eq!(memories, ["The user lives in Lisbon.", "The user uses Vim."]);
        let lisbon = memories[0].clone();
        let update = Update {
            remember: strings(&["the user uses vim.", "The user lives in\nBerlin."]),
            forget: vec![1, 0, 7],
        };
        assert!(update.apply(&[&lisbon], &mut memories));
        assert_eq!(memories, ["The user uses Vim.", "The user lives in Berlin."]);

        let nothing = Update {
            remember: strings(&[" "]),
            forget: vec![],
        };
        assert!(!nothing.apply(&[], &mut memories));
    }

    #[test]
    fn rewording_keeps_one_memory_saying_the_new_words() {
        let mut memories = strings(&["Uses vim.", "Lives in Lisbon.", "Likes tea."]);
        assert!(reword(&mut memories, "Lives in Lisbon.", "Lives in Berlin.".into()));
        assert_eq!(memories, ["Uses vim.", "Lives in Berlin.", "Likes tea."]);
        assert!(reword(&mut memories, "Uses vim.", "Uses Vim.".into()), "case alone");
        assert!(reword(&mut memories, "Likes tea.", "uses vim.".into()), "merges");
        assert_eq!(memories, ["Uses Vim.", "Lives in Berlin."]);
        assert!(reword(&mut memories, "Gone.", "Likes coffee.".into()), "saves anew");
        assert!(!reword(&mut memories, "Gone.", "likes coffee.".into()));
        assert_eq!(memories, ["Uses Vim.", "Lives in Berlin.", "Likes coffee."]);
    }
}
