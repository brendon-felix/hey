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
use rig_core::tool::DynamicTool;
use rig_tools::Spill;

use crate::config::{self, Providers};
use crate::gate::Gate;
use crate::permissions::{Approval, Permissions, Reporter};
use crate::protocol::ModelInfo;
use crate::route::Routes;

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

/// An agent on `model` with `tools`, each running where `routes` says. Its
/// tool calls ask as `permissions` say, through `reporter`, and run side by
/// side where that is safe: the gate sees every tool the agent ends up
/// with, however it was added.
pub fn builder<R: Reporter>(
    model: &Model,
    tools: Vec<DynamicTool>,
    routes: Arc<Routes>,
    permissions: Arc<Permissions>,
    reporter: Arc<R>,
) -> AgentBuilder {
    let server = ToolServer::new().run();
    let approval = Approval::new(permissions, reporter, Gate::new(server.clone()), routes);
    AgentBuilder::new(model.inner.clone())
        .record_content_telemetry(crate::log::content())
        .tool_server_handle(server)
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
    Ok(summarizer.prompt(request).await?.output())
}

/// This process's working directory, or `unknown`.
pub fn working_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("unknown"))
}
