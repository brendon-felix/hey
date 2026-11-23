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
use rig_agent::agent::AgentBuilder;
use rig_core::completion::ToolAnnotations;
use rig_core::message::ToolName;
use rig_core::message::{AssistantContent, Message, UserContent};
use rig_core::tool::{
    DynamicTool, Tool, ToolContext, ToolErrorKind, ToolExecutionError, ToolOutput, args_schema,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::agent::{self, Model};
use crate::config::{self, Providers};
use vectors::Vectors;

const HEADER: &str = "# Memories\n\n\
What hey remembers across conversations, oldest first. Edit freely: hey keeps \
the lines that start with `- ` and rewrites the file as it learns.\n\n";

const LEARNING_PROMPT: &str = "\
You keep the long-term memory of hey, a terminal assistant: the few things it should know about \
its user in later, unrelated conversations. Read the exchange, then call these tools as needed:
- `remember(text)`: save a fact about the user that will still hold months from now and helps in \
many conversations, not just ones like this: who they are, their background and work, lasting \
preferences and how they like to be answered, the machines they use and what each is for, and \
the ongoing projects they work on, what each is and where it lives. One short, self-contained \
sentence about the user per call.
- `revise(number, text)`: reword the saved memory with that number, when it needs correcting or \
updating.
- `forget(number)`: delete the saved memory with that number, when the exchange shows it to be \
wrong or outdated.
Do not remember:
- hey itself: its configuration, models, tools, MCP servers, limits or behavior.
- The state of a machine, directory, repository or codebase: what is installed, configured, \
checked out or available, what a directory holds, how code is structured or where things are \
defined. These change, and the assistant can look them up when it needs them.
- The task at hand: its details, progress, findings, bugs and fixes.
- What is already saved, general knowledge, the assistant's suggestions the user did not adopt, \
or secrets such as passwords, keys and tokens.
When unsure whether a fact is lasting and broadly useful, leave it out. Most exchanges teach \
nothing lasting: then call no tool and reply `nothing to save`.";

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
            Err(error) => {
                Err(error).with_context(|| format!("cannot read memories {}", self.file.display()))
            }
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
        let ops: Arc<Mutex<Vec<Op>>> = Arc::default();
        let agent = AgentBuilder::new(model.completion())
            .preamble(LEARNING_PROMPT)
            .dynamic_tools(tools(&shown, &ops))
            .default_max_turns(4)
            .build();
        let request = learning_request(&shown, &transcript);
        crate::retry::model("learning", || async {
            ops.lock().unwrap_or_else(PoisonError::into_inner).clear();
            agent.prompt(request.clone()).await
        })
        .await
        .context("cannot learn from the answer")?;
        let ops = std::mem::take(&mut *ops.lock().unwrap_or_else(PoisonError::into_inner));
        self.change(|memories| apply(ops, memories))?;
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

/// The name the model calls the tool that recalls memories by.
pub const TOOL: &str = "recall_memory";

/// The tool that recalls what is remembered about the user, on demand
/// rather than in every prompt.
pub struct Recall {
    pub memory: Arc<Memory>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallArgs {
    /// What to recall, in a few words or a sentence, such as the topic of the request.
    pub query: String,
}

impl Tool for Recall {
    const NAME: &'static str = TOOL;
    type Args = RecallArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Recalls saved facts about the user (background, preferences, machines, projects) that \
         are closest in meaning to the query. Call it when a request may depend on who the user \
         is or how they like things done; they may be outdated, and what the user says now takes \
         precedence."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<RecallArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: RecallArgs,
    ) -> Result<String, ToolExecutionError> {
        let recalled =
            self.memory.recall(&args.query).await.map_err(|error| {
                ToolExecutionError::new(ToolErrorKind::Other, format!("{error:#}"))
            })?;
        if recalled.memories.is_empty() {
            return Ok("Nothing is remembered yet.".into());
        }
        let note = recalled
            .fallback
            .map(|error| format!("(the newest, as closeness could not be judged: {error:#})\n"))
            .unwrap_or_default();
        let list: Vec<String> = recalled
            .memories
            .iter()
            .map(|memory| format!("- {memory}"))
            .collect();
        Ok(format!("{note}{}", list.join("\n")))
    }
}

/// One change the model learning from an exchange asked for.
#[derive(Debug, PartialEq)]
enum Op {
    Remember(String),
    Revise { old: String, new: String },
    Forget(String),
}

/// Makes `ops` to `memories`, in order. False when nothing changed.
fn apply(ops: Vec<Op>, memories: &mut Vec<String>) -> bool {
    let mut changed = false;
    for op in ops {
        changed |= match op {
            Op::Remember(text) => add(memories, one_line(&text)),
            Op::Revise { old, new } => reword(memories, &old, one_line(&new)),
            Op::Forget(old) => {
                let count = memories.len();
                memories.retain(|saved| *saved != old);
                memories.len() != count
            }
        };
    }
    changed
}

/// The tools the model learning from an exchange changes memory with; each
/// tells it what it did, or what was wrong, so it can correct itself. The
/// changes are collected in `ops`, to be saved once it is done.
fn tools(shown: &[&String], ops: &Arc<Mutex<Vec<Op>>>) -> Vec<DynamicTool> {
    let shown: Arc<Vec<String>> = Arc::new(shown.iter().map(|memory| (*memory).clone()).collect());
    let tool = |name: &str,
                description: &str,
                parameters: serde_json::Value,
                run: fn(&[String], &serde_json::Value) -> Result<Op, String>| {
        let (shown, ops) = (shown.clone(), ops.clone());
        DynamicTool::new(
            ToolName::new(name).expect("a valid tool name"),
            description,
            parameters,
            move |arguments| {
                let reply = match run(&shown, &arguments) {
                    Ok(op) => {
                        ops.lock().unwrap_or_else(PoisonError::into_inner).push(op);
                        "ok".to_owned()
                    }
                    Err(problem) => problem,
                };
                Box::pin(async move { Ok(ToolOutput::text(reply)) })
            },
        )
    };
    let text = json!({ "type": "string", "description": "One short, self-contained sentence about the user." });
    let number =
        json!({ "type": "integer", "description": "The number of a saved memory, as listed." });
    vec![
        tool(
            "remember",
            "Save a new lasting fact about the user.",
            json!({ "type": "object", "properties": { "text": text }, "required": ["text"] }),
            |_, arguments| match argument(arguments, "text") {
                Some(text) => Ok(Op::Remember(text)),
                None => Err("`text` is required".to_owned()),
            },
        ),
        tool(
            "revise",
            "Reword the saved memory with this number.",
            json!({ "type": "object", "properties": { "number": number, "text": text }, "required": ["number", "text"] }),
            |shown, arguments| {
                let old = numbered(shown, arguments)?;
                match argument(arguments, "text") {
                    Some(new) => Ok(Op::Revise { old, new }),
                    None => Err("`text` is required".to_owned()),
                }
            },
        ),
        tool(
            "forget",
            "Delete the saved memory with this number.",
            json!({ "type": "object", "properties": { "number": number }, "required": ["number"] }),
            |shown, arguments| numbered(shown, arguments).map(Op::Forget),
        ),
    ]
}

fn argument(arguments: &serde_json::Value, name: &str) -> Option<String> {
    let text = one_line(arguments.get(name)?.as_str()?);
    (!text.is_empty()).then_some(text)
}

/// The saved memory that `arguments` number, or why there is none.
fn numbered(shown: &[String], arguments: &serde_json::Value) -> Result<String, String> {
    let number = arguments.get("number").and_then(|number| {
        number
            .as_u64()
            .or_else(|| number.as_str()?.trim().parse().ok())
    });
    number
        .and_then(|number| shown.get(usize::try_from(number).ok()?.checked_sub(1)?))
        .cloned()
        .ok_or_else(|| {
            format!(
                "no saved memory has that number; the numbers are 1 to {}",
                shown.len()
            )
        })
}

/// Saves `memory` unless it is empty or already saved, ignoring case.
fn add(memories: &mut Vec<String>, memory: String) -> bool {
    let known = memories
        .iter()
        .any(|saved| saved.eq_ignore_ascii_case(&memory));
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
    fn changes_apply_in_order_and_skip_known_memories() {
        let mut memories = parse(&format!(
            "{HEADER}- The user lives in Lisbon.\n- The user  uses\tVim.\nnot a memory\n"
        ));
        assert_eq!(
            memories,
            ["The user lives in Lisbon.", "The user uses Vim."]
        );
        let ops = vec![
            Op::Forget("The user lives in Lisbon.".into()),
            Op::Remember("the user uses vim.".into()),
            Op::Remember("The user lives in\nBerlin.".into()),
            Op::Revise {
                old: "The user uses Vim.".into(),
                new: "The user uses Neovim.".into(),
            },
        ];
        assert!(apply(ops, &mut memories));
        assert_eq!(
            memories,
            ["The user uses Neovim.", "The user lives in Berlin."]
        );
        assert!(!apply(vec![Op::Remember(" ".into())], &mut memories));
    }

    #[test]
    fn tools_name_memories_by_their_listed_number() {
        let shown = strings(&["Uses vim.", "Likes tea."]);
        assert_eq!(
            numbered(&shown, &json!({ "number": 2 })).unwrap(),
            "Likes tea."
        );
        assert_eq!(
            numbered(&shown, &json!({ "number": "1" })).unwrap(),
            "Uses vim."
        );
        assert!(numbered(&shown, &json!({ "number": 3 })).is_err());
        assert!(numbered(&shown, &json!({ "number": 0 })).is_err());
    }

    #[test]
    fn rewording_keeps_one_memory_saying_the_new_words() {
        let mut memories = strings(&["Uses vim.", "Lives in Lisbon.", "Likes tea."]);
        assert!(reword(
            &mut memories,
            "Lives in Lisbon.",
            "Lives in Berlin.".into()
        ));
        assert_eq!(memories, ["Uses vim.", "Lives in Berlin.", "Likes tea."]);
        assert!(
            reword(&mut memories, "Uses vim.", "Uses Vim.".into()),
            "case alone"
        );
        assert!(
            reword(&mut memories, "Likes tea.", "uses vim.".into()),
            "merges"
        );
        assert_eq!(memories, ["Uses Vim.", "Lives in Berlin."]);
        assert!(
            reword(&mut memories, "Gone.", "Likes coffee.".into()),
            "saves anew"
        );
        assert!(!reword(&mut memories, "Gone.", "likes coffee.".into()));
        assert_eq!(memories, ["Uses Vim.", "Lives in Berlin.", "Likes coffee."]);
    }
}
