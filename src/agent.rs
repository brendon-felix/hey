/* -------------------------------------------------------------------------- */
/*                                  agent.rs                                  */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use rig_agent::agent::{Agent, AgentBuilder, StreamingResult};
use rig_core::DynModel;
use rig_core::catalog::Catalog;
use rig_core::operation::Completion;
use rig_core::providers::registry::ConnectOptions;
use rig_core::tool::Tool;
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Spill, Write};

use crate::config::{self, Config, Providers};
use crate::mcp;
use crate::permissions::{Approval, Origin, Permission, Permissions};
use crate::render;
use crate::sessions::{SessionId, Sessions};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are hey, an assistant in the user's terminal. \
Use your tools to read, search and edit files and to run commands whenever the answer depends \
on the user's machine or project, rather than guessing. Keep answers short: they are read in a \
terminal and rendered as Markdown.";

const BUILTIN_TOOLS: [&str; 8] = [
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

    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn context_window(&self) -> Option<u32> {
        self.inner.spec().and_then(|spec| spec.context_window)
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

pub struct Assistant {
    agent: Agent,
    model: Model,
    permissions: Arc<Permissions>,
}

impl Assistant {
    pub fn new(
        config: &Config,
        model: Model,
        system_prompt: &str,
        sessions: &Sessions,
        mcp_tools: Vec<mcp::Tool>,
    ) -> Self {
        let builtin = BUILTIN_TOOLS.map(|name| (name, &Origin::Builtin));
        let external = mcp_tools.iter().map(|tool| (tool.name.as_str(), &tool.origin));
        let permissions = Arc::new(Permissions::resolve(
            builtin.into_iter().chain(external),
            &config.tools,
        ));

        let mut builder = with_builtin_tools(AgentBuilder::new(model.inner.clone()))
            .dynamic_tools(mcp_tools.into_iter().map(|tool| tool.tool).collect())
            .name("hey")
            .preamble(system_prompt)
            .append_preamble(&environment())
            .default_max_turns(config.max_turns)
            .memory(sessions.store())
            .add_hook(Approval(permissions.clone()));
        if let Some(max_tokens) = config.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }
        Self {
            agent: builder.build(),
            model,
            permissions,
        }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn set_model(&mut self, model: Model) {
        self.agent.set_model(model.inner.clone());
        self.model = model;
    }

    pub fn permissions(&self) -> BTreeMap<String, Permission> {
        self.permissions.snapshot()
    }

    /// Answers `prompt` in `session`, whose log the run reads before its first
    /// model call and appends to once it finishes.
    pub fn run(&self, prompt: &str, session: &SessionId) -> StreamingResult {
        self.agent
            .prompt(prompt)
            .conversation(session.as_str())
            .stream()
    }
}

fn with_builtin_tools(builder: AgentBuilder) -> AgentBuilder {
    // `edit` and `write` refuse to clobber a file changed since `read` saw it,
    // which takes all three sharing what was seen.
    let seen = Seen::default();
    let shell = Shell {
        live: Some(Live::new(render::live_output)),
        spill: config::cache_dir().map(|dir| Spill(dir.join("spill"))),
        ..Shell::default()
    };
    builder
        .tool(Read {
            seen: seen.clone(),
            ..Read::default()
        })
        .tool(Write {
            seen: seen.clone(),
            ..Write::default()
        })
        .tool(Edit {
            seen,
            ..Edit::default()
        })
        .tool(List::default())
        .tool(Search::default())
        .tool(Move::default())
        .tool(Delete::default())
        .tool(shell)
}

fn environment() -> String {
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
