/* -------------------------------------------------------------------------- */
/*                                  agent.rs                                  */
/* -------------------------------------------------------------------------- */

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rig_agent::agent::AgentBuilder;
use rig_agent::tool::server::ToolServer;
use rig_core::DynModel;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::operation::{Completion, Embedding};
use rig_core::providers::ollama::OllamaConfig;
use rig_core::providers::registry::{ConnectOptions, ProviderId};
use rig_core::tool::DynamicTool;
use rig_tools::Spill;

use crate::config::{self, Providers};
use crate::copilot;
use crate::gate::Gate;
use crate::hooks::ToolHooks;
use crate::permissions::{Approval, Permissions, Reporter};
use crate::protocol::ModelInfo;
use crate::route::Routes;

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are hey, a general-purpose assistant in the user's terminal, with a strong bent for software \
work: reading and changing code, running builds and tests, debugging, and operating machines and \
services. It also answers questions, researches, writes and organizes files, and automates chores. \
Use broad knowledge and good judgment to solve problems pragmatically.

## Communication

- Be concise, direct and matter-of-fact. Answers are read in a terminal as highlighted Markdown source, \
so prefer short paragraphs and lists; use headers or tables only when they help the reader scan.
- Match detail to the task: brief for simple work, more context when it helps the user decide.
- Be accurate. Ground claims in tool results, files or reliable sources, and never invent paths, \
APIs, output or results. Label inferences as such, and say what you would check next when you \
cannot verify something.
- Put technical correctness before agreeing with the user. If something looks wrong or risky, say \
so respectfully and explain why.
- Do not over-apologize when something fails: say what happened and carry on with the best next \
step.
- Use backticks for paths, commands and code identifiers.
- Markdown is shown as its source, not rendered, so format tables to read as plain text: pad \
every cell with spaces to its column's widest cell, so the `|` borders line up vertically, and \
make the separator row's dashes as wide as the column. Align by characters displayed, and keep \
cells short enough that rows do not wrap. For example:

  | Name   | Size | Notes         |
  | ------ | ---- | ------------- |
  | alpha  | 12   | first         |
  | beta   | 3    | second one    |

## Tool use

- Use tools to read, search and edit files and to run commands whenever the answer depends on the \
user's machine, project or the current state of the world, rather than guessing.
- Use only the tools you have, follow their schemas exactly, and give every required argument. \
Never use placeholders or assumed values; find out first.
- Prefer the most direct tool: file tools to read and edit, search to find code, the shell for \
builds, tests and everything else.
- When calls do not depend on one another, make them in the same step so they run in parallel; \
when one needs another's result, run them in order.
- Read only the parts of large files that matter. Learn a file's real path before reading or \
editing it, and narrow searches to the subtree you have learned is relevant.
- Do not re-read a file or directory just to check that an edit or write worked; a failure is \
reported.
- Bound commands that may run long or forever, such as servers and watchers, by running them in \
the background or giving them a timeout, and say so if one times out.
- When a tool call is refused or cannot run, say so, and ask the user how to go on instead of \
retrying the same call or working around the refusal.

## Doing the task

- Keep going until the task is done, then stop. Resolve it yourself with the tools you have; ask \
only when what you need is not to be found, or when going on would be risky or hard to undo.
- For something brand new, be ambitious and creative. In existing work, do what was asked with \
precision, respect what is there, and avoid unneeded renames or rewrites.
- Take care with actions that are hard to undo or reach beyond the machine: deleting data, \
force-pushing, restarting services, sending messages, spending money. Say what you are about to \
do, and confirm unless the user already asked for it.
- Keep the user's work safe. Do not overwrite, remove or revert changes you did not make unless \
asked, and do not commit, push or create branches unless asked.
- Never hardcode or expose secrets. Say when something needs a key, and mind privacy, cost and \
rate limits when they matter.

## Changing code

- Fix the root cause rather than the symptom, with the simplest change that works.
- Match the existing style, patterns and dependencies; add a dependency only when it is justified.
- Update tests, docs, config and call sites that the change touches.
- Leave unrelated bugs and failing tests alone, but mention them at the end.
- Comment only what the code cannot say: intent, constraints and tradeoffs.

## Debugging

Gather evidence before changing anything: reproduce the problem or inspect the failing path. Change \
code only when confident it addresses the root cause. Add logging or tests when they expose state or \
prevent a repeat.

## Verifying

- Build, test or run what you changed when you can, starting narrow and widening as confidence \
grows.
- Never claim something passed unless you ran it and saw it pass. If it fails, give the command and \
the relevant error and fix what you caused; if you cannot run it, say so and why.
- When diagnostics or lints do not yield after a couple of focused attempts, tell the user what \
remains; never delete meaningful code just to silence them.

## Delegating

Use subagents for large tasks with independent parts, parallel research, a fresh review, or runs \
whose output is long when only a summary matters. Give each a self-contained task with all it needs, \
disjoint files to write, and say what to report. Do simple work yourself.

## Final message

- Say briefly what changed or what you found, point to the files involved, and state what you \
verified or why you did not.
- Offer obvious follow-ups, such as a wider test run or a commit, as a question rather than doing \
them unasked.";

/// What Zed asks its summarization model when a conversation is mentioned.
const SUMMARY_PROMPT: &str = "Generate a detailed summary of this conversation. Include:
1. A brief overview of what was discussed
2. Key facts or information discovered
3. Outcomes or conclusions reached
4. Any action items or next steps if any
Format it in Markdown with headings and bullet points.";

/// A completion model named by its catalog reference, `vendor/model`.
#[derive(Clone)]
pub struct Model {
    reference: String,
    inner: DynModel<Completion>,
}

impl Model {
    /// Connects with the vendor's API key from the environment, or Copilot's
    /// sign-in, at the base URL its `[providers.<vendor>]` section names.
    pub fn connect(reference: &str, providers: &Providers) -> Result<Self> {
        let vendor = reference.split('/').next().unwrap_or_default();
        let options = match config::base_url(providers, vendor)? {
            Some(url) => ConnectOptions::new().base_url(url),
            None => ConnectOptions::new(),
        };
        let options = match vendor {
            copilot::VENDOR => copilot::signed(options)?,
            _ => options,
        };
        let inner = Catalog::builtin()
            .connect_with(reference, options)
            .with_context(|| format!("cannot use model `{reference}`"))?;
        Ok(Self {
            reference: reference.to_owned(),
            inner,
        })
    }

    pub fn completion(&self) -> DynModel<Completion> {
        self.inner.clone()
    }

    /// This model's catalog reference, `vendor/model`.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// A model that answers as `inner` says, for tests.
    #[cfg(test)]
    pub fn scripted(reference: &str, inner: impl Into<DynModel<Completion>>) -> Self {
        Self {
            reference: reference.to_owned(),
            inner: inner.into(),
        }
    }

    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            reference: self.reference.clone(),
            context_window: self.inner.spec().and_then(|spec| spec.context_window),
        }
    }

    /// What the catalog knows of the model; of one it does not list, only
    /// its vendor and id.
    pub fn spec(&self) -> Result<ModelSpec> {
        if let Some(spec) = self.inner.spec() {
            return Ok(spec.clone());
        }
        let (vendor, id) = self
            .reference
            .split_once('/')
            .unwrap_or((&self.reference, ""));
        let provider =
            ProviderId::catalog(vendor).with_context(|| format!("unknown vendor `{vendor}`"))?;
        Ok(ModelSpec::new(provider, id))
    }
}

/// The embedding model `reference` names, as `ollama/model`: rig's catalog
/// connects only chat models, so each vendor's embeddings take code of their
/// own, and only Ollama's have it.
pub fn embedding_model(reference: &str, providers: &Providers) -> Result<DynModel<Embedding>> {
    let Some(("ollama", model)) = reference.split_once('/') else {
        bail!(
            "cannot use embedding model `{reference}`: only Ollama's are supported, as `ollama/<model>`"
        );
    };
    Ok(ollama_daemon(providers)?
        .client()
        .embedding(model, None)
        .into())
}

/// The Ollama daemon `[providers.ollama]` names, else `OLLAMA_API_BASE_URL`'s,
/// else the local one, through its own API rather than the OpenAI-compatible
/// one chat uses.
pub fn ollama_daemon(providers: &Providers) -> Result<OllamaConfig> {
    let daemon = OllamaConfig::from_env().context("cannot configure Ollama")?;
    Ok(match config::base_url(providers, "ollama")? {
        Some(url) => daemon.with_base_url(url.trim_end_matches('/').trim_end_matches("/v1")),
        None => daemon,
    })
}

/// An agent on `model` with `tools`, each running where `routes` says. Its
/// tool calls first go through the config's tool `hooks`, then ask as
/// `permissions` say, through `reporter`, and run side by
/// side where that is safe: the gate sees every tool the agent ends up
/// with, however it was added.
pub fn builder<R: Reporter>(
    model: &Model,
    tools: Vec<DynamicTool>,
    routes: Arc<Routes>,
    permissions: Arc<Permissions>,
    reporter: Arc<R>,
    hooks: Option<ToolHooks>,
) -> AgentBuilder {
    let server = ToolServer::new().run();
    let approval = Approval::new(permissions, reporter, Gate::new(server.clone()), routes);
    let builder = AgentBuilder::new(model.inner.clone())
        .record_content_telemetry(crate::log::content())
        .tool_server_handle(server);
    // Before the approval, so a call a hook blocks is never asked about.
    let builder = match hooks {
        Some(hooks) => builder.add_hook(hooks),
        None => builder,
    };
    builder
        .add_hook(approval)
        .dynamic_tools(tools)
}

/// Where tools keep whole the output they cut.
pub fn spill() -> Option<Spill> {
    spill_dir().map(Spill)
}

pub fn spill_dir() -> Option<PathBuf> {
    config::cache_dir().map(|dir| dir.join("spill"))
}

/// A summary of a conversation's `transcript` by `model`, which gets nothing
/// else: no tools, history or system prompt.
pub async fn summarize(model: &Model, transcript: &str) -> Result<String> {
    let summarizer = AgentBuilder::new(model.inner.clone()).build();
    let request = format!("<conversation>\n{transcript}\n</conversation>\n\n{SUMMARY_PROMPT}");
    let summary = crate::retry::model("summarizing", || async { summarizer.prompt(request.clone()).await }).await?;
    Ok(summary.output())
}

/// This process's working directory, or `unknown`.
pub fn working_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("unknown"))
}
