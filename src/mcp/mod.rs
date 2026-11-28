/* -------------------------------------------------------------------------- */
/*                                 mcp/mod.rs                                 */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rig_core::message::ToolName;
use rig_core::tool::DynamicTool;
use rig_rmcp::{McpConnection, McpTool};
use rmcp::ServiceExt;
use rmcp::model::{ClientConfig, ContentBlock, GetPromptRequestParams, ResourceContents};
use rmcp::service::ClientInitializeError;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClientTransportConfig,
};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{McpServer, McpTransport, expand, expand_headers};
use crate::permissions::Origin;

pub mod deferred;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What every MCP tool's name starts with, as the model sees it.
pub const PREFIX: &str = "mcp__";

/// A tool from an MCP server, offered to the model under a name that cannot
/// collide with the built-in tools or another server's.
#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub origin: Origin,
    pub tool: DynamicTool,
}

/// What a server offers once connected.
#[derive(Clone)]
pub struct Connected {
    pub tools: Vec<Tool>,
    /// What the server says it is for, in a line.
    pub summary: Option<String>,
    pub prompts: Vec<Prompt>,
    /// Whether it lists resources the model may read.
    pub resources: bool,
    /// Whether its tools are left out of what the model is told.
    pub deferred: bool,
    pub connection: McpConnection,
}

/// Connects to one server, failing once it takes longer than
/// [`CONNECT_TIMEOUT`]. Servers connect each on their own, so a slow one
/// holds up no other.
pub async fn connect_one(name: &str, server: &McpServer) -> Result<Connected> {
    tokio::time::timeout(CONNECT_TIMEOUT, connect(name, server))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {CONNECT_TIMEOUT:?}")))
}

async fn connect(server: &str, config: &McpServer) -> Result<Connected> {
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
            client
                .serve(transport)
                .await
                .map_err(initialize_error)?
                .into()
        }
        McpTransport::Http { url, headers } => {
            let config = StreamableHttpClientTransportConfig::with_uri(expand(url)?)
                .custom_headers(expand_headers(headers)?);
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
    let tools = tools
        .into_iter()
        .map(|tool| namespaced(server, tool))
        .collect::<Result<_>>()?;
    let info = connection.peer().peer_info();
    let capabilities = info.as_ref().map(|info| &info.capabilities);
    let prompts = match capabilities.and_then(|capabilities| capabilities.prompts.as_ref()) {
        Some(_) => connection
            .peer()
            .list_all_prompts()
            .await
            .map(|prompts| {
                prompts
                    .into_iter()
                    .map(|prompt| Prompt {
                        server: server.to_owned(),
                        ..Prompt::from(prompt)
                    })
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let summary = info.as_ref().and_then(|info| {
        let described = info
            .server_info
            .as_ref()
            .and_then(|server| server.description.clone());
        described.or_else(|| info.instructions.clone())
    });
    Ok(Connected {
        tools,
        summary: summary.as_deref().and_then(first_line),
        prompts,
        resources: capabilities.is_some_and(|capabilities| capabilities.resources.is_some()),
        deferred: config.deferred,
        connection,
    })
}

/// A prompt a server offers, which the user invokes as `/mcp:<server>:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    /// Its arguments' names, required ones first.
    pub arguments: Vec<String>,
}

impl From<rmcp::model::Prompt> for Prompt {
    fn from(prompt: rmcp::model::Prompt) -> Self {
        let mut arguments = prompt.arguments.unwrap_or_default();
        arguments.sort_by_key(|argument| !argument.required.unwrap_or(false));
        Self {
            server: String::new(),
            name: prompt.name,
            description: prompt.description,
            arguments: arguments
                .into_iter()
                .map(|argument| argument.name)
                .collect(),
        }
    }
}

/// The prompt of `prompts` that `word`, typed after `/`, invokes:
/// `mcp:<server>:<prompt>`, or `<server>:<prompt>`.
pub fn invoked<'a>(prompts: &'a [Prompt], word: &str) -> Option<&'a Prompt> {
    let word = word.strip_prefix("mcp:").unwrap_or(word);
    let (server, name) = word.split_once(':')?;
    prompts
        .iter()
        .find(|prompt| prompt.server == server && prompt.name == name)
}

impl Prompt {
    /// How the user invokes it.
    pub fn command(&self) -> String {
        format!("mcp:{}:{}", self.server, self.name)
    }

    /// `text`, typed after the command, as the prompt's arguments: `key=value`
    /// pairs, or all of it as the first argument.
    pub fn arguments(&self, text: &str) -> serde_json::Map<String, Value> {
        let text = text.trim();
        let pairs: Option<Vec<(&str, &str)>> = text
            .split_whitespace()
            .map(|pair| pair.split_once('='))
            .collect();
        match (pairs, self.arguments.first()) {
            (Some(pairs), _) if !pairs.is_empty() => pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                .collect(),
            (_, Some(first)) if !text.is_empty() => {
                std::iter::once((first.clone(), Value::String(text.to_owned()))).collect()
            }
            _ => serde_json::Map::new(),
        }
    }
}

/// The messages a server's prompt `name` gives for `arguments`, as one text.
pub async fn get_prompt(
    connection: &McpConnection,
    name: &str,
    arguments: serde_json::Map<String, Value>,
) -> Result<String> {
    let request = GetPromptRequestParams::new(name).with_arguments(arguments);
    let result = connection.peer().get_prompt(request).await?;
    let texts: Vec<String> = result
        .messages
        .iter()
        .filter_map(|message| match &message.content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            ContentBlock::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    if texts.is_empty() {
        bail!("the prompt gave no text");
    }
    Ok(texts.join("\n\n"))
}

/// The first line of `text`, at most 160 characters.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut line: String = line.chars().take(160).collect();
    if line.len() < text.trim().len() && !line.ends_with('.') {
        line.push('…');
    }
    Some(line)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(arguments: &[&str]) -> Prompt {
        Prompt {
            server: "git".into(),
            name: "review".into(),
            description: None,
            arguments: arguments.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    #[test]
    fn prompt_arguments_are_pairs_or_the_whole_text() {
        let review = prompt(&["branch", "focus"]);
        assert_eq!(review.command(), "mcp:git:review");
        let pairs = review.arguments("branch=main focus=tests");
        assert_eq!(pairs["branch"], "main");
        assert_eq!(pairs["focus"], "tests");
        let whole = review.arguments(" the parser work ");
        assert_eq!(whole["branch"], "the parser work");
        assert!(review.arguments("").is_empty());
        assert!(prompt(&[]).arguments("anything").is_empty());
    }

    #[test]
    fn prompts_are_invoked_with_or_without_mcp() {
        let prompts = [prompt(&[])];
        assert!(invoked(&prompts, "mcp:git:review").is_some());
        assert!(invoked(&prompts, "git:review").is_some());
        assert!(invoked(&prompts, "git:other").is_none());
        assert!(invoked(&prompts, "review").is_none());
    }

    #[test]
    fn summaries_keep_the_first_line() {
        assert_eq!(
            first_line("\nSearch docs.\nMore."),
            Some("Search docs.".into())
        );
        assert_eq!(first_line("Search docs\nMore"), Some("Search docs…".into()));
        assert_eq!(first_line("  "), None);
    }
}

/// `mcp__<server>__<tool>`, in the characters and length providers accept.
pub fn model_name(server: &str, tool: &str) -> String {
    format!("{PREFIX}{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
