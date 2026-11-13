/* -------------------------------------------------------------------------- */
/*                                  agent.rs                                  */
/* -------------------------------------------------------------------------- */

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rig_agent::agent::AgentBuilder;
use rig_agent::tool::server::ToolServer;
use rig_core::DynModel;
use rig_core::catalog::Catalog;
use rig_core::operation::{Completion, Embedding};
use rig_core::providers::ollama::OllamaConfig;
use rig_core::providers::registry::ConnectOptions;
use rig_core::tool::Tool;
use rig_tools::process::Launcher;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Spill, Write};

use crate::config::{self, Providers};
use crate::gate::Gate;
use crate::permissions::{Approval, Permissions, Reporter};
use crate::protocol::ModelInfo;
use crate::skills;
use crate::web::Web;

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

/// The tools every agent has, by the names their permissions are keyed by:
/// `skill` is offered only when there are skills to load, and the web tools
/// as `web` offers them.
pub fn builtin_tools(web: &Web) -> impl Iterator<Item = &'static str> {
    [
        Read::NAME,
        List::NAME,
        Search::NAME,
        Write::NAME,
        Edit::NAME,
        Move::NAME,
        Delete::NAME,
        Shell::NAME,
        skills::TOOL,
    ]
    .into_iter()
    .chain(web.tools())
}

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

    pub fn completion(&self) -> DynModel<Completion> {
        self.inner.clone()
    }

    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            reference: self.reference.clone(),
            context_window: self.inner.spec().and_then(|spec| spec.context_window),
        }
    }
}

/// The embedding model `reference` names, as `ollama/model`: rig's catalog
/// connects only chat models, so each vendor's embeddings take code of their
/// own, and only Ollama's have it. The daemon is the one `[providers.ollama]`
/// names, else `OLLAMA_API_BASE_URL`'s, else the local one.
pub fn embedding_model(reference: &str, providers: &Providers) -> Result<DynModel<Embedding>> {
    let Some(("ollama", model)) = reference.split_once('/') else {
        bail!("cannot use embedding model `{reference}`: only Ollama's are supported, as `ollama/<model>`");
    };
    let mut daemon = OllamaConfig::from_env().context("cannot configure Ollama")?;
    if let Some(url) = providers.get("ollama").and_then(|p| p.base_url.as_deref()) {
        // The configured URL is the OpenAI-compatible API, which chat uses;
        // embeddings use the daemon's own API, at its root.
        let url = config::expand(url)?;
        daemon = daemon.with_base_url(url.trim_end_matches('/').trim_end_matches("/v1"));
    }
    Ok(daemon.client().embedding(model, None).into())
}
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

/// Where the built-in tools work.
pub struct Place {
    workspace: Arc<dyn Workspace>,
    /// The directory relative paths start from and commands run in; `None`
    /// is this process's working directory.
    base: Option<PathBuf>,
    /// Whether paths outside `base` are refused.
    confine: bool,
    /// Where a running command's output goes as it comes.
    live: Option<Live>,
}

impl Place {
    /// Anywhere on this machine, from the working directory.
    pub fn here(live: Live) -> Self {
        Self {
            workspace: Arc::new(Local),
            base: None,
            confine: false,
            live: Some(live),
        }
    }

    /// Only within `dir`, on `workspace`. Commands start there, but what
    /// they do is not confined.
    pub fn within(dir: PathBuf, workspace: Arc<dyn Workspace>) -> Self {
        Self {
            workspace,
            base: Some(dir),
            confine: true,
            live: None,
        }
    }
}

/// An agent on `model` with the built-in tools, working in `place` and
/// reading the web through `web`. `edit` and `write` refuse to clobber a
/// file changed since `read` saw it, which takes all three sharing what was
/// `seen`. Its tool calls ask as `permissions` say, through `reporter`, and
/// run side by side where that is safe: the gate sees every tool the agent
/// ends up with, however it was added.
pub fn builder<R: Reporter>(
    model: &Model,
    seen: &Seen,
    place: Place,
    web: &Web,
    permissions: Arc<Permissions>,
    reporter: Arc<R>,
) -> AgentBuilder {
    let Place {
        workspace,
        base,
        confine,
        live,
    } = place;
    let launcher = match &base {
        Some(dir) => Launcher::default().in_dir(dir),
        None => Launcher::default(),
    };
    let shell = Shell {
        workspace: workspace.clone(),
        launcher,
        live,
        spill: spill(),
        confine,
        ..Shell::default()
    };
    let tools = ToolServer::new().run();
    let approval = Approval::new(permissions, reporter, Gate::new(tools.clone()));
    let builder = AgentBuilder::new(model.inner.clone())
        .tool_server_handle(tools)
        .add_hook(approval)
        .tool(Read {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
            seen: seen.clone(),
        })
        .tool(Write {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
            seen: seen.clone(),
        })
        .tool(Edit {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
            seen: seen.clone(),
        })
        .tool(List {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
        })
        .tool(Search {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
        })
        .tool(Move {
            workspace: workspace.clone(),
            base: base.clone(),
            confine,
        })
        .tool(Delete {
            workspace,
            base,
            confine,
        })
        .tool(shell)
        .tool(web.fetch());
    match web.search() {
        Some(search) => builder.tool(search),
        None => builder,
    }
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
    Ok(summarizer.prompt(request).await?.output())
}

/// This process's working directory, or `unknown`.
pub fn working_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("unknown"))
}
