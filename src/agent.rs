/* -------------------------------------------------------------------------- */
/*                                  agent.rs                                  */
/* -------------------------------------------------------------------------- */

use anyhow::{Context, Result};
use rig_agent::agent::AgentBuilder;
use rig_core::DynModel;
use rig_core::catalog::Catalog;
use rig_core::operation::Completion;
use rig_core::providers::registry::ConnectOptions;
use rig_core::tool::Tool;
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Spill, Write};

use crate::config::{self, Providers};
use crate::protocol::ModelInfo;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are hey, an assistant in the user's terminal. \
Use your tools to read, search and edit files and to run commands whenever the answer depends \
on the user's machine or project, rather than guessing. Keep answers short: they are read in a \
terminal and rendered as Markdown.";

/// What Zed asks its summarization model when a conversation is mentioned.
const SUMMARY_PROMPT: &str = "Generate a detailed summary of this conversation. Include:
1. A brief overview of what was discussed
2. Key facts or information discovered
3. Outcomes or conclusions reached
4. Any action items or next steps if any
Format it in Markdown with headings and bullet points.";

pub const BUILTIN_TOOLS: [&str; 8] = [
    Read::NAME,
    List::NAME,
    Search::NAME,
    Write::NAME,
    Edit::NAME,
    Move::NAME,
    Delete::NAME,
    Shell::NAME,
];

/// A completion model named by its catalog reference, `vendor/model`.
#[derive(Clone)]
pub struct Model {
    reference: String,
    inner: DynModel<Completion>,
}

impl Model {
    /// Connects with the vendor's API key from the environment, at the base
    /// URL its `[providers.<vendor>]` section names.
    pub fn connect(reference: &str, providers: &Providers) -> Result<Self> {
        let vendor = reference.split('/').next().unwrap_or_default();
        let options = match providers.get(vendor).and_then(|p| p.base_url.as_deref()) {
            Some(url) => ConnectOptions::new().base_url(config::expand(url)?),
            None => ConnectOptions::new(),
        };
        let inner = Catalog::builtin()
            .connect_with(reference, options)
            .with_context(|| format!("cannot use model `{reference}`"))?;
        Ok(Self {
            reference: reference.to_owned(),
            inner,
        })
    }

    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            reference: self.reference.clone(),
            context_window: self.inner.spec().and_then(|spec| spec.context_window),
        }
    }
}

/// Models with tool support whose vendor has an API key in the environment.
pub fn available_models() -> Vec<String> {
    let has_key = |vendor: &rig_core::providers::registry::ProviderId| {
        vendor
            .api_key_envs()
            .iter()
            .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
    };
    Catalog::builtin()
        .iter()
        .filter(|spec| spec.tools && !spec.deprecated && has_key(&spec.provider))
        .map(|spec| format!("{}/{}", spec.provider.vendor(), spec.id))
        .collect()
}

/// An agent on `model` with the built-in tools. `edit` and `write` refuse to
/// clobber a file changed since `read` saw it, which takes all three sharing
/// what was `seen`; `live` receives a running command's output.
pub fn builder(model: &Model, seen: &Seen, live: Live) -> AgentBuilder {
    let shell = Shell {
        live: Some(live),
        spill: config::cache_dir().map(|dir| Spill(dir.join("spill"))),
        ..Shell::default()
    };
    AgentBuilder::new(model.inner.clone())
        .tool(Read {
            seen: seen.clone(),
            ..Read::default()
        })
        .tool(Write {
            seen: seen.clone(),
            ..Write::default()
        })
        .tool(Edit {
            seen: seen.clone(),
            ..Edit::default()
        })
        .tool(List::default())
        .tool(Search::default())
        .tool(Move::default())
        .tool(Delete::default())
        .tool(shell)
}

/// A summary of a conversation's `transcript` by `model`, which gets nothing
/// else: no tools, history or system prompt.
pub async fn summarize(model: &Model, transcript: &str) -> Result<String> {
    let summarizer = AgentBuilder::new(model.inner.clone()).build();
    let request = format!("<conversation>\n{transcript}\n</conversation>\n\n{SUMMARY_PROMPT}");
    Ok(summarizer.prompt(request).await?.output())
}

pub fn environment() -> String {
    let cwd = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!(
        "\nEnvironment:\n- Working directory: {cwd}\n- Platform: {} ({})\n- Date: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        chrono::Local::now().format("%Y-%m-%d"),
    )
}
