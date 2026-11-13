/* -------------------------------------------------------------------------- */
/*                                 mcp/mod.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rig_core::message::ToolName;
use rig_core::tool::DynamicTool;
use rig_rmcp::{McpConnection, McpTool};
use rmcp::ServiceExt;
use rmcp::model::ClientConfig;
use rmcp::service::ClientInitializeError;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClientTransportConfig,
};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

use crate::config::{McpServer, McpTransport, expand};
use crate::permissions::Origin;

mod results;

pub use results::ResultLimit;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What every MCP tool's name starts with, as the model sees it.
const PREFIX: &str = "mcp__";

/// A tool from an MCP server, offered to the model under a name that cannot
/// collide with the built-in tools or another server's.
pub struct Tool {
    pub name: String,
    pub origin: Origin,
    pub tool: DynamicTool,
}

pub struct Server {
    pub name: String,
    pub tools: Result<Vec<Tool>>,
}

/// Connects every server at once; one that fails is reported, not fatal.
pub async fn connect_all(servers: &BTreeMap<String, McpServer>) -> Vec<Server> {
    let connecting = servers.iter().map(|(name, server)| async move {
        let tools = tokio::time::timeout(CONNECT_TIMEOUT, connect(name, server))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {CONNECT_TIMEOUT:?}")));
        Server {
            name: name.clone(),
            tools,
        }
    });
    futures::future::join_all(connecting).await
}

async fn connect(server: &str, config: &McpServer) -> Result<Vec<Tool>> {
    let client = ClientConfig::default();
    let connection: McpConnection = match &config.transport {
        McpTransport::Stdio { command, args, env } => {
            let mut command = tokio::process::Command::new(expand(command)?);
            command.args(args);
            for (key, value) in env {
                command.env(key, expand(value)?);
            }
            // Out of the terminal's foreground group, so Ctrl+C stops the
            // run without also killing the server.
            #[cfg(unix)]
            command.process_group(0);
            // A server's logs would interleave with the conversation.
            let (transport, _) = TokioChildProcess::builder(command)
                .stderr(Stdio::null())
                .spawn()
                .context("cannot start the server")?;
            client.serve(transport).await.map_err(initialize_error)?.into()
        }
        McpTransport::Http { url, headers } => {
            let config = StreamableHttpClientTransportConfig::with_uri(expand(url)?)
                .custom_headers(header_map(headers)?);
            let transport = StreamableHttpClientTransport::from_config(config);
            let client = client.serve(transport).await.map_err(initialize_error);
            client.map_err(explain_sign_in)?.into()
        }
    };
    let tools = connection.tools().await?;
    let tools = match &config.tools {
        Some(wanted) => only(tools, wanted)?,
        None => tools,
    };
    tools.into_iter().map(|tool| namespaced(server, tool)).collect()
}

/// The `wanted` tools of those a server offers, every one of which it must
/// offer, so a misspelt name is not silently left out.
fn only(tools: Vec<McpTool>, wanted: &BTreeSet<String>) -> Result<Vec<McpTool>> {
    let missing: Vec<&str> = wanted
        .iter()
        .filter(|name| !tools.iter().any(|tool| tool.name() == name.as_str()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        let offered: Vec<&str> = tools.iter().map(McpTool::name).collect();
        bail!(
            "`tools` names {}, which the server does not offer; it offers {}",
            missing.join(", "),
            offered.join(", ")
        );
    }
    Ok(tools
        .into_iter()
        .filter(|tool| wanted.contains(tool.name()))
        .collect())
}

/// rmcp does not chain a transport failure as its error's source, which
/// hides the cause (such as a DNS failure) behind the transport's type name.
fn initialize_error(error: ClientInitializeError) -> anyhow::Error {
    match error {
        ClientInitializeError::TransportError { error, context } => {
            anyhow::Error::from_boxed(error.error).context(context)
        }
        other => other.into(),
    }
}

/// A server that answers 401 wants an OAuth sign-in, which hey cannot do;
/// say how to connect with a key instead.
fn explain_sign_in(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| cause.is::<AuthRequiredError>()) {
        anyhow::anyhow!(
            "the server requires a sign-in, which hey does not support yet; \
             set `headers = {{ Authorization = \"Bearer ${{YOUR_KEY_VAR}}\" }}` instead"
        )
    } else {
        error
    }
}

fn namespaced(server: &str, tool: McpTool) -> Result<Tool> {
    let mcp_name = tool.name().to_owned();
    let name = model_name(server, &mcp_name);
    let tool = tool.with_name(ToolName::new(name.clone())?);
    Ok(Tool {
        origin: Origin::Mcp {
            server: server.to_owned(),
            tool: mcp_name,
        },
        tool: DynamicTool::try_from(tool)?,
        name,
    })
}

/// `mcp__<server>__<tool>`, in the characters and length providers accept.
fn model_name(server: &str, tool: &str) -> String {
    format!("{PREFIX}{server}__{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .take(64)
        .collect()
}

fn header_map(
    headers: &BTreeMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = http::HeaderName::try_from(name.as_str())
                .with_context(|| format!("invalid header name {name:?}"))?;
            let value = http::HeaderValue::try_from(expand(value)?)
                .with_context(|| format!("invalid value for header {name}"))?;
            Ok((name, value))
        })
        .collect()
}
