/* -------------------------------------------------------------------------- */
/*                                 toolbox.rs                                 */
/* -------------------------------------------------------------------------- */

//! The tools a machine runs for agents: the built-in ones, the web's and
//! its MCP servers'. A host has its own, and so does a remote client, whose
//! tools the host's agents can call on the client's machine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use rig_core::completion::ToolDefinition;
use rig_core::message::ToolName;
use rig_core::serve::adapters::ToolAdapter;
use rig_core::tool::{DynamicTool, Tool, ToolErrorKind, ToolExecutionError};
use rig_tools::process::Launcher;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Write};
use tokio::sync::watch;

use crate::agent;
use crate::config::{self, Config};
use crate::lenient;
use crate::mcp::{self, deferred, deferred::Deferred};
use crate::permissions::{Later, Origin, Permission, Permissions};
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

/// How long an answer waits for MCP servers still connecting before it
/// goes on without their tools.
pub const PATIENCE: Duration = Duration::from_secs(3);

pub struct Toolbox {
    web: Web,
    mcp: Arc<Mcp>,
}

/// The MCP servers, which connect in the background, each on its own: what
/// has connected so far is what a run is given, so a slow or dead server
/// holds up neither the prompt nor the others.
struct Mcp {
    web: Web,
    tools: config::Tools,
    /// Every server's name, in order.
    names: Vec<String>,
    outcomes: Mutex<BTreeMap<String, Result<mcp::Connected, String>>>,
    state: RwLock<Arc<Connected>>,
    /// How many servers have yet to connect or fail.
    pending: watch::Sender<usize>,
}

/// What the MCP servers that connected offer.
#[derive(Default)]
struct Connected {
    tools: Vec<mcp::Tool>,
    deferred: Arc<Deferred>,
    /// The servers that list resources, by name.
    resources: Arc<[(String, McpConnection)]>,
    prompts: Vec<mcp::Prompt>,
    connections: Vec<(String, McpConnection)>,
    servers: Vec<ServerReport>,
    /// The permission of every tool known, for conversations made before
    /// the tools were.
    rules: BTreeMap<String, Permission>,
}

impl Later for Mcp {
    fn rule(&self, tool: &str) -> Option<Permission> {
        self.snapshot().rules.get(tool).copied()
    }

    fn rules(&self) -> BTreeMap<String, Permission> {
        self.snapshot().rules.clone()
    }
}

impl Mcp {
    fn snapshot(&self) -> Arc<Connected> {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Takes in how connecting to `name` went, then offers what has
    /// connected.
    fn done(&self, name: &str, outcome: Result<mcp::Connected>) {
        if let Err(error) = &outcome {
            tracing::warn!(server = name, error = format!("{error:#}"), "MCP server unavailable");
            render::notice(&format!("MCP server `{name}` unavailable: {error:#}"));
        }
        let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        outcomes.insert(name.to_owned(), outcome.map_err(|error| format!("{error:#}")));
        self.publish(&outcomes);
    }

    fn publish(&self, outcomes: &BTreeMap<String, Result<mcp::Connected, String>>) {
        let state = Arc::new(self.offer(outcomes));
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = state;
        self.pending.send_replace(self.names.len() - outcomes.len());
    }

    /// What `outcomes` offer: a server's tools are deferred when its config
    /// says so, or all are once they number more than `defer_mcp_above`.
    fn offer(&self, outcomes: &BTreeMap<String, Result<mcp::Connected, String>>) -> Connected {
        let mut connected = Connected::default();
        let total: usize = outcomes
            .values()
            .filter_map(|outcome| outcome.as_ref().ok())
            .map(|server| server.tools.len())
            .sum();
        let defer_all = self.tools.defer_mcp_above.is_some_and(|above| total > above);
        let mut deferred_servers = Vec::new();
        let mut resources = Vec::new();
        for name in &self.names {
            let report = match outcomes.get(name) {
                None => ServerReport {
                    name: name.clone(),
                    deferred: false,
                    tools: Ok(0),
                    pending: true,
                },
                Some(Err(error)) => ServerReport {
                    name: name.clone(),
                    deferred: false,
                    tools: Err(error.clone()),
                    pending: false,
                },
                Some(Ok(offered)) => {
                    let deferred = defer_all || offered.deferred;
                    if deferred {
                        deferred_servers.push(deferred::Server {
                            name: name.clone(),
                            summary: offered.summary.clone(),
                            tools: offered.tools.clone(),
                        });
                    } else {
                        connected.tools.extend(offered.tools.iter().cloned());
                    }
                    if offered.resources {
                        resources.push((name.clone(), offered.connection.clone()));
                    }
                    connected.prompts.extend(offered.prompts.iter().cloned());
                    connected
                        .connections
                        .push((name.clone(), offered.connection.clone()));
                    ServerReport {
                        name: name.clone(),
                        deferred,
                        tools: Ok(offered.tools.len()),
                        pending: false,
                    }
                }
            };
            connected.servers.push(report);
        }
        connected.deferred = Arc::new(Deferred::new(deferred_servers));
        connected.resources = resources.into();
        let origins = origins(&self.web, &connected);
        connected.rules = Permissions::resolved(
            origins.iter().map(|(name, origin)| (name.as_str(), origin)),
            &self.tools,
        );
        connected
    }
}

/// Every tool's name and where it comes from, which its permission is
/// configured by.
fn origins(web: &Web, connected: &Connected) -> Vec<(String, Origin)> {
    let builtin = BUILTIN.into_iter().chain(web.tools());
    let builtin = builtin.map(|name| (name.to_owned(), Origin::Builtin));
    let stand_ins = [deferred::SEARCH, deferred::CALL]
        .into_iter()
        .filter(|_| !connected.deferred.is_empty())
        .chain(
            [deferred::RESOURCES, deferred::READ_RESOURCE]
                .into_iter()
                .filter(|_| !connected.resources.is_empty()),
        )
        .map(|name| (name.to_owned(), Origin::Builtin));
    let mcp = connected.tools.iter().chain(connected.deferred.tools());
    let mcp = mcp.map(|tool| (tool.name.clone(), tool.origin.clone()));
    builtin.chain(stand_ins).chain(mcp).collect()
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
    /// The tools `config` gives this machine. Its MCP servers connect in the
    /// background, each on its own, and their tools join as they do.
    pub fn new(config: &Config) -> Result<Self> {
        let web = Web::new(config.tools.result_bytes)?;
        let names: Vec<String> = config.mcp_servers.keys().cloned().collect();
        let mcp = Arc::new(Mcp {
            web: web.clone(),
            tools: config.tools.clone(),
            pending: watch::channel(names.len()).0,
            names,
            outcomes: Mutex::default(),
            state: RwLock::default(),
        });
        mcp.publish(&BTreeMap::new());
        for (name, server) in &config.mcp_servers {
            let (mcp, name, server) = (mcp.clone(), name.clone(), server.clone());
            tokio::spawn(async move {
                let outcome = mcp::connect_one(&name, &server).await;
                mcp.done(&name, outcome);
            });
        }
        Ok(Self { web, mcp })
    }

    /// Waits until every MCP server has connected or failed, but for at
    /// most `patience`: what a run starts with is then what is known.
    pub async fn settled(&self, patience: Duration) {
        let mut pending = self.mcp.pending.subscribe();
        _ = tokio::time::timeout(patience, pending.wait_for(|pending| *pending == 0)).await;
    }

    /// Where the permissions of tools that connected later are found.
    pub fn later(&self) -> Arc<dyn Later> {
        self.mcp.clone()
    }

    pub fn servers(&self) -> Vec<ServerReport> {
        self.mcp.snapshot().servers.clone()
    }

    /// What the model is told of the deferred MCP servers.
    pub fn preamble(&self) -> Option<String> {
        self.mcp.snapshot().deferred.preamble()
    }

    /// The prompts the MCP servers offer so far.
    pub fn prompts(&self) -> Vec<mcp::Prompt> {
        self.mcp.snapshot().prompts.clone()
    }

    /// The text of `prompt`, for the `arguments` typed after it.
    pub async fn prompt(&self, prompt: &mcp::Prompt, arguments: &str) -> Result<String> {
        let (server, name) = (&prompt.server, &prompt.name);
        let connected = self.mcp.snapshot();
        let (_, connection) = connected
            .connections
            .iter()
            .find(|(of, _)| of == server)
            .with_context(|| format!("no MCP server `{server}`"))?;
        mcp::get_prompt(connection, name, prompt.arguments(arguments)).await
    }

    /// Every tool's name and where it comes from, which its permission is
    /// configured by.
    pub fn origins(&self) -> Vec<(String, Origin)> {
        origins(&self.web, &self.mcp.snapshot())
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
        let shell = backgrounded(dynamic(shell), background.clone());
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
            shell.clone(),
            aliased(shell, ALIAS),
            background,
            dynamic(ProcessOutput),
            dynamic(ProcessList),
            dynamic(ProcessKill),
            dynamic(Table),
            dynamic(self.web.fetch()),
        ];
        let search = self.web.search().map(dynamic);
        let connected = self.mcp.snapshot();
        let mcp = connected.tools.iter().map(|tool| tool.tool.clone());
        let stand_ins = deferred::tools(&connected.deferred)
            .into_iter()
            .chain(deferred::resource_tools(&connected.resources));
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

/// The name other agents give `shell`, which models call it by.
pub const ALIAS: &str = "bash";

/// The tool a call to `name` is, as permissions and routing know it: `bash`
/// is `shell`.
pub fn canonical(name: &str) -> &str {
    if name == ALIAS { Shell::NAME } else { name }
}

/// `tool` under another `name`, taking the same arguments, so that a model
/// that calls it by that name is not told it does not exist.
fn aliased(tool: DynamicTool, name: &str) -> DynamicTool {
    let definition = tool.definition();
    let description = format!("Same as `{}`.", definition.name);
    DynamicTool::new_with_context(
        ToolName::new(name).expect("alias names are valid"),
        description,
        definition.parameters,
        move |context, args| {
            let tool = tool.clone();
            Box::pin(async move { tool.run_with(context, args).await })
        },
    )
    .with_annotations(definition.annotations)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hung_server_does_not_hold_up_the_toolbox() {
        // `sleep` starts but never speaks MCP, so connecting waits for it.
        let config: Config = toml::from_str(
            r#"
            [mcp_servers.hung]
            command = "sleep"
            args = ["30"]
        "#,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let toolbox = Toolbox::new(&config).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        // A run waits only as long as it is told to for what is pending.
        let waited = std::time::Instant::now();
        toolbox.settled(Duration::from_millis(200)).await;
        assert!(waited.elapsed() < Duration::from_secs(2));
        let servers = toolbox.servers();
        assert!(servers[0].pending);
        assert!(toolbox.definitions().iter().any(|tool| tool.name == "read"));
    }

    #[tokio::test]
    async fn a_server_that_cannot_start_neither_delays_nor_blocks_the_toolbox() {
        let config: Config = toml::from_str(
            r#"
            [mcp_servers.broken]
            command = "/nonexistent/hey-test-server"
        "#,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let toolbox = Toolbox::new(&config).unwrap();
        // Known at once, still connecting.
        assert!(started.elapsed() < Duration::from_millis(500));
        let servers = toolbox.servers();
        assert_eq!(servers.len(), 1);
        assert!(servers[0].pending || servers[0].tools.is_err());
        // It fails fast, and what then runs has the built-in tools.
        toolbox.settled(Duration::from_secs(5)).await;
        let servers = toolbox.servers();
        assert!(!servers[0].pending);
        assert!(servers[0].tools.is_err());
        assert!(toolbox.definitions().iter().any(|tool| tool.name == "read"));
    }

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
