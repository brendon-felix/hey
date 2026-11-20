/* -------------------------------------------------------------------------- */
/*                                 toolbox.rs                                 */
/* -------------------------------------------------------------------------- */

//! The tools a machine runs for agents: the built-in ones, the web's and
//! its MCP servers'. A host has its own, and so does a remote client, whose
//! tools the host's agents can call on the client's machine.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rig_core::completion::ToolDefinition;
use rig_core::message::ToolName;
use rig_core::serve::adapters::ToolAdapter;
use rig_core::tool::{DynamicTool, Tool, ToolErrorKind, ToolExecutionError};
use rig_tools::process::Launcher;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Write};

use crate::agent;
use crate::config::Config;
use crate::lenient;
use crate::mcp::{self, deferred, deferred::Deferred};
use crate::permissions::Origin;
use crate::processes::{ProcessKill, ProcessList, ProcessOutput, SpawnProcess};
use crate::protocol::ServerReport;
use crate::render;
use crate::table::Table;
use crate::web::Web;
use rig_rmcp::McpConnection;

/// The built-in tools every machine has, by name.
pub const BUILTIN: [&str; 13] = [
    Read::NAME,
    List::NAME,
    Search::NAME,
    Write::NAME,
    Edit::NAME,
    Move::NAME,
    Delete::NAME,
    Shell::NAME,
    SpawnProcess::NAME,
    ProcessOutput::NAME,
    ProcessList::NAME,
    ProcessKill::NAME,
    Table::NAME,
];

/// This machine's name: `HEY_MACHINE`, else its host name without the
/// domain, as `hostname -s` prints it. Rules in a config shared between
/// machines are kept apart by it.
pub fn machine_name() -> String {
    if let Some(name) = std::env::var("HEY_MACHINE")
        .ok()
        .filter(|name| !name.is_empty())
    {
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
    deferred: Arc<Deferred>,
    /// The servers that list resources, by name.
    resources: Arc<[(String, McpConnection)]>,
    prompts: Vec<mcp::Prompt>,
    connections: Vec<(String, McpConnection)>,
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
        let connected = connect_mcp(config).await;
        Ok(Self {
            web: Web::new(config.tools.result_bytes)?,
            mcp: connected.tools,
            deferred: Arc::new(Deferred::new(connected.deferred)),
            resources: connected.resources.into(),
            prompts: connected.prompts,
            connections: connected.connections,
            servers: connected.reports,
        })
    }

    pub fn servers(&self) -> &[ServerReport] {
        &self.servers
    }

    /// What the model is told of the deferred MCP servers.
    pub fn preamble(&self) -> Option<String> {
        self.deferred.preamble()
    }

    /// The prompts the MCP servers offer.
    pub fn prompts(&self) -> &[mcp::Prompt] {
        &self.prompts
    }

    /// The text of `prompt`, for the `arguments` typed after it.
    pub async fn prompt(&self, prompt: &mcp::Prompt, arguments: &str) -> Result<String> {
        let (server, name) = (&prompt.server, &prompt.name);
        let (_, connection) = self
            .connections
            .iter()
            .find(|(of, _)| of == server)
            .with_context(|| format!("no MCP server `{server}`"))?;
        mcp::get_prompt(connection, name, prompt.arguments(arguments)).await
    }

    /// Every tool's name and where it comes from, which its permission is
    /// configured by.
    pub fn origins(&self) -> impl Iterator<Item = (&str, &Origin)> {
        let builtin = BUILTIN.into_iter().chain(self.web.tools());
        let builtin = builtin.map(|name| (name, &Origin::Builtin));
        let stand_ins = [deferred::SEARCH, deferred::CALL]
            .into_iter()
            .filter(|_| !self.deferred.is_empty())
            .chain(
                [deferred::RESOURCES, deferred::READ_RESOURCE]
                    .into_iter()
                    .filter(|_| !self.resources.is_empty()),
            )
            .map(|name| (name, &Origin::Builtin));
        let mcp = self.mcp.iter().chain(self.deferred.tools());
        let mcp = mcp.map(|tool| (tool.name.as_str(), &tool.origin));
        builtin.chain(stand_ins).chain(mcp)
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
        let spawn = || SpawnProcess {
            workspace: workspace.clone(),
            launcher: launcher.clone(),
            confine,
            spill: agent::spill(),
        };
        let background = dynamic(spawn());
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
            backgrounded(dynamic(shell), background.clone()),
            background,
            dynamic(ProcessOutput),
            dynamic(ProcessList),
            dynamic(ProcessKill),
            dynamic(Table),
            dynamic(self.web.fetch()),
        ];
        let search = self.web.search().map(dynamic);
        let mcp = self.mcp.iter().map(|tool| tool.tool.clone());
        let stand_ins = deferred::tools(&self.deferred)
            .into_iter()
            .chain(deferred::resource_tools(&self.resources));
        builtin
            .into_iter()
            .chain(search)
            .chain(mcp)
            .chain(stand_ins)
            .collect()
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
        let args = lenient::normalize(T::NAME, args);
        Box::pin(async move {
            tool.run_with(context, args)
                .await
                .map_err(|error| briefly(T::NAME, error))
        })
    })
    .with_annotations(annotations)
}

/// The longest part of an argument error the model is shown.
const ARGUMENT_ERROR: usize = 300;

/// `error`, when it is arguments that did not parse, without the arguments
/// it echoes: a long edit sent back whole costs the model its context and
/// does not say what was wrong.
fn briefly(tool: &str, error: ToolExecutionError) -> ToolExecutionError {
    if error.kind() != ToolErrorKind::InvalidArgs
        || !error
            .message()
            .starts_with("failed to parse tool arguments")
    {
        return error;
    }
    let message = error.message();
    let end = message
        .char_indices()
        .nth(ARGUMENT_ERROR)
        .map_or(message.len(), |(at, _)| at);
    let cut = if end < message.len() { "…" } else { "" };
    let hint = match tool {
        "edit" => {
            " `edits` must be a JSON array of {\"old_text\", \"new_text\"} objects (a \
             string holding that JSON works only if it is valid JSON); or send `path`, \
             `old_text` and `new_text` as top-level fields for one change. For a large \
             change, make several smaller edits, or use `write`."
        }
        _ => " Send the arguments as one JSON object that matches the tool's schema.",
    };
    ToolExecutionError::invalid_args(format!("{}{cut}.{hint}", &message[..end]))
}

/// `shell`, which sends a call that asks for `run_in_background`, as Claude
/// Code's Bash tool takes it, to `spawn_process`. The schema stays `shell`'s.
fn backgrounded(shell: DynamicTool, spawn: DynamicTool) -> DynamicTool {
    let definition = shell.definition();
    DynamicTool::new_with_context(
        definition.name,
        definition.description,
        definition.parameters,
        move |context, args| {
            let background = args
                .get("run_in_background")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let tool = if background { &spawn } else { &shell };
            let tool = tool.clone();
            Box::pin(async move { tool.run_with(context, args).await })
        },
    )
    .with_annotations(definition.annotations)
}

/// What the MCP servers that connected offer, and how each connection went.
#[derive(Default)]
struct McpConnected {
    tools: Vec<mcp::Tool>,
    deferred: Vec<deferred::Server>,
    resources: Vec<(String, McpConnection)>,
    prompts: Vec<mcp::Prompt>,
    connections: Vec<(String, McpConnection)>,
    reports: Vec<ServerReport>,
}

/// Connects every server; a server's tools are deferred when its config
/// says so, or all are once they number more than `defer_mcp_above`.
async fn connect_mcp(config: &Config) -> McpConnected {
    let mut connected = McpConnected::default();
    let servers = mcp::connect_all(&config.mcp_servers).await;
    let total: usize = servers
        .iter()
        .filter_map(|server| server.connected.as_ref().ok())
        .map(|server| server.tools.len())
        .sum();
    let defer_all = config
        .tools
        .defer_mcp_above
        .is_some_and(|above| total > above);
    for server in servers {
        let outcome = server.connected.map(|offered| {
            let count = offered.tools.len();
            let deferred = defer_all || offered.deferred;
            if deferred {
                connected.deferred.push(deferred::Server {
                    name: server.name.clone(),
                    summary: offered.summary,
                    tools: offered.tools,
                });
            } else {
                connected.tools.extend(offered.tools);
            }
            if offered.resources {
                connected
                    .resources
                    .push((server.name.clone(), offered.connection.clone()));
            }
            connected.prompts.extend(offered.prompts);
            connected
                .connections
                .push((server.name.clone(), offered.connection));
            (count, deferred)
        });
        if let Err(error) = &outcome {
            render::notice(&format!(
                "MCP server `{}` unavailable: {error:#}",
                server.name
            ));
        }
        connected.reports.push(ServerReport {
            name: server.name,
            deferred: outcome.as_ref().is_ok_and(|(_, deferred)| *deferred),
            tools: outcome
                .map(|(count, _)| count)
                .map_err(|error| format!("{error:#}")),
        });
    }
    connected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_argument_error_is_cut_and_says_what_an_edit_takes() {
        let long = format!("failed to parse tool arguments: {}", "x".repeat(5000));
        let error = briefly("edit", ToolExecutionError::invalid_args(long));
        assert!(error.message().len() < 800);
        assert!(error.message().contains("array"));
        let other = ToolExecutionError::invalid_args("no such file");
        assert_eq!(briefly("edit", other).message(), "no such file");
    }
}
