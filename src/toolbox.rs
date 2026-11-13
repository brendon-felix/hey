/* -------------------------------------------------------------------------- */
/*                                 toolbox.rs                                 */
/* -------------------------------------------------------------------------- */

//! The tools a machine runs for agents: the built-in ones, the web's and
//! its MCP servers'. A host has its own, and so does a remote client, whose
//! tools the host's agents can call on the client's machine.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rig_core::completion::ToolDefinition;
use rig_core::message::ToolName;
use rig_core::serve::adapters::ToolAdapter;
use rig_core::tool::{DynamicTool, Tool};
use rig_tools::process::Launcher;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Write};

use crate::agent;
use crate::config::Config;
use crate::mcp;
use crate::permissions::Origin;
use crate::protocol::ServerReport;
use crate::render;
use crate::web::Web;

/// The built-in tools every machine has, by name.
pub const BUILTIN: [&str; 8] = [
    Read::NAME,
    List::NAME,
    Search::NAME,
    Write::NAME,
    Edit::NAME,
    Move::NAME,
    Delete::NAME,
    Shell::NAME,
];

/// This machine's name: `HEY_MACHINE`, else its host name without the
/// domain, as `hostname -s` prints it. Rules in a config shared between
/// machines are kept apart by it.
pub fn machine_name() -> String {
    if let Some(name) = std::env::var("HEY_MACHINE").ok().filter(|name| !name.is_empty()) {
        return name;
    }
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    match host.split('.').next() {
        Some(short) if !short.is_empty() => short.to_owned(),
        _ => "localhost".into(),
    }
}

pub struct Toolbox {
    web: Web,
    mcp: Vec<mcp::Tool>,
    servers: Vec<ServerReport>,
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
    pub fn here(live: Option<Live>) -> Self {
        Self::on(Arc::new(Local), live)
    }

    /// Anywhere `workspace` lets them, from the working directory.
    pub fn on(workspace: Arc<dyn Workspace>, live: Option<Live>) -> Self {
        Self {
            workspace,
            base: None,
            confine: false,
            live,
        }
    }
}

impl Toolbox {
    /// The tools `config` gives this machine, its MCP servers connected.
    pub async fn new(config: &Config) -> Result<Self> {
        let (mcp, servers) = connect_mcp(config).await;
        Ok(Self {
            web: Web::new(config.tools.result_bytes)?,
            mcp,
            servers,
        })
    }

    pub fn servers(&self) -> &[ServerReport] {
        &self.servers
    }

    /// Every tool's name and where it comes from, which its permission is
    /// configured by.
    pub fn origins(&self) -> impl Iterator<Item = (&str, &Origin)> {
        let builtin = BUILTIN.into_iter().chain(self.web.tools());
        let builtin = builtin.map(|name| (name, &Origin::Builtin));
        let mcp = self.mcp.iter().map(|tool| (tool.name.as_str(), &tool.origin));
        builtin.chain(mcp)
    }

    /// The tools of one agent working in `place`. `edit` and `write` refuse
    /// to clobber a file changed since `read` saw it, which takes all three
    /// sharing what was `seen`.
    pub fn tools(&self, seen: &Seen, place: Place) -> Vec<DynamicTool> {
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
            spill: agent::spill(),
            confine,
            ..Shell::default()
        };
        let builtin = [
            dynamic(Read {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
                seen: seen.clone(),
            }),
            dynamic(Write {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
                seen: seen.clone(),
            }),
            dynamic(Edit {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
                seen: seen.clone(),
            }),
            dynamic(List {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
            }),
            dynamic(Search {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
            }),
            dynamic(Move {
                workspace: workspace.clone(),
                base: base.clone(),
                confine,
            }),
            dynamic(Delete {
                workspace,
                base,
                confine,
            }),
            dynamic(shell),
            dynamic(self.web.fetch()),
        ];
        let search = self.web.search().map(dynamic);
        let mcp = self.mcp.iter().map(|tool| tool.tool.clone());
        builtin.into_iter().chain(search).chain(mcp).collect()
    }

    /// What each tool is, as an agent elsewhere is told of it.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools(&Seen::default(), Place::here(None))
            .iter()
            .map(DynamicTool::definition)
            .collect()
    }
}

/// `tool` as one defined at runtime, which a host can route to the machine
/// a call names, keeping what its annotations say it does.
pub fn dynamic<T: Tool + 'static>(tool: T) -> DynamicTool {
    let name = ToolName::new(T::NAME).expect("built-in tool names are valid");
    let (description, parameters) = (tool.description(), tool.parameters());
    let annotations = tool.annotations();
    let tool = Arc::new(ToolAdapter::new(tool));
    DynamicTool::new_with_context(name, description, parameters, move |context, args| {
        let tool = tool.clone();
        Box::pin(async move { tool.run_with(context, args).await })
    })
    .with_annotations(annotations)
}

/// The tools of every server that connected, and how each connection went.
async fn connect_mcp(config: &Config) -> (Vec<mcp::Tool>, Vec<ServerReport>) {
    let mut tools = Vec::new();
    let mut reports = Vec::new();
    for server in mcp::connect_all(&config.mcp_servers).await {
        let outcome = server.tools.map(|server_tools| {
            let count = server_tools.len();
            tools.extend(server_tools);
            count
        });
        if let Err(error) = &outcome {
            render::notice(&format!(
                "MCP server `{}` unavailable: {error:#}",
                server.name
            ));
        }
        reports.push(ServerReport {
            name: server.name,
            tools: outcome.map_err(|error| format!("{error:#}")),
        });
    }
    (tools, reports)
}
