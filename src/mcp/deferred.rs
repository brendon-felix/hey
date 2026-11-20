/* -------------------------------------------------------------------------- */
/*                              mcp/deferred.rs                               */
/* -------------------------------------------------------------------------- */

//! The tools that stand in for what MCP servers offer without describing
//! all of it on every call: `tool_search` and `mcp_call` for the tools of
//! deferred servers, `mcp_resources` and `mcp_read_resource` for resources.
//!
//! A deferred tool is called through `mcp_call` rather than added to the
//! agent once found: the tools an agent is given stay the same through a
//! run, its subagents and a remote client's lent tools, and the provider's
//! prompt cache keeps working. A call is permitted under the deferred
//! tool's own `mcp:<server>:<tool>` rules, as [`called`] names it.

use std::sync::Arc;

use rig_core::completion::{ToolAnnotations, ToolDefinition};
use rig_core::message::ToolName;
use rig_core::tool::{DynamicTool, ToolErrorKind, ToolExecutionError, ToolOutput};
use rig_rmcp::McpConnection;
use rmcp::model::ResourceContents;
use serde_json::{Value, json};

use super::{Tool, model_name};

pub const SEARCH: &str = "tool_search";
pub const CALL: &str = "mcp_call";
pub const RESOURCES: &str = "mcp_resources";
pub const READ_RESOURCE: &str = "mcp_read_resource";

/// Most tools one search gives back.
const MAX_FOUND: usize = 8;

/// A server whose tools are deferred.
pub struct Server {
    pub name: String,
    pub summary: Option<String>,
    pub tools: Vec<Tool>,
}

/// The deferred servers of a machine.
#[derive(Default)]
pub struct Deferred {
    servers: Vec<Server>,
}

/// The name a deferred tool's permission is kept under, `mcp__<server>__<tool>`,
/// when `args` call one through `mcp_call`.
pub fn called(tool: &str, args: &str) -> Option<String> {
    if tool != CALL {
        return None;
    }
    let args: Value = serde_json::from_str(args).ok()?;
    let server = args.get("server")?.as_str()?;
    let tool = args.get("tool")?.as_str()?;
    Some(model_name(server, tool))
}

impl Deferred {
    pub fn new(servers: Vec<Server>) -> Self {
        Self { servers }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.servers.iter().flat_map(|server| &server.tools)
    }

    /// What the model is told of the deferred servers: each with what it is
    /// for and its tools' names.
    pub fn preamble(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let servers: String = self
            .servers
            .iter()
            .map(|server| {
                let names: Vec<&str> = server.tools.iter().map(|tool| mcp_tool(tool)).collect();
                let summary = server
                    .summary
                    .as_deref()
                    .map(|summary| format!(" {summary}"))
                    .unwrap_or_default();
                format!(
                    "\n- `{}`:{summary} Tools: {}.",
                    server.name,
                    names.join(", ")
                )
            })
            .collect();
        Some(format!(
            "\nDeferred MCP servers, whose tools you are not shown until you look them up: \
             `{SEARCH}` gives their parameters, then `{CALL}` calls one.{servers}"
        ))
    }

    /// The tools matching `query`, best first.
    fn search(&self, query: &str, server: Option<&str>) -> Vec<(&str, &Tool)> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut scored: Vec<(usize, &str, &Tool)> = self
            .servers
            .iter()
            .filter(|of| server.is_none_or(|server| of.name == server))
            .flat_map(|of| of.tools.iter().map(move |tool| (of.name.as_str(), tool)))
            .filter_map(|(server, tool)| {
                let name = mcp_tool(tool).to_lowercase();
                let description = tool.tool.definition().description.to_lowercase();
                let score: usize = words
                    .iter()
                    .map(|word| {
                        match (
                            name.contains(word.as_str()),
                            description.contains(word.as_str()),
                        ) {
                            (true, _) => 3,
                            (false, true) => 1,
                            _ => 0,
                        }
                    })
                    .sum();
                (score > 0 || words.is_empty()).then_some((score, server, tool))
            })
            .collect();
        scored.sort_by_key(|(score, ..)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .take(MAX_FOUND)
            .map(|(_, server, tool)| (server, tool))
            .collect()
    }

    fn find(&self, server: &str, tool: &str) -> Option<&Tool> {
        self.servers
            .iter()
            .find(|of| of.name == server)?
            .tools
            .iter()
            .find(|of| mcp_tool(of) == tool)
    }
}

/// The server's own name for `tool`.
fn mcp_tool(tool: &Tool) -> &str {
    match &tool.origin {
        crate::permissions::Origin::Mcp { tool, .. } => tool,
        crate::permissions::Origin::Builtin => &tool.name,
    }
}

fn name(name: &str) -> ToolName {
    ToolName::new(name).expect("these tool names are valid")
}

fn reading() -> ToolAnnotations {
    ToolAnnotations::default()
        .with_read_only(true)
        .with_open_world(false)
}

fn invalid(error: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::invalid_args(error.into())
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `tool_search` and `mcp_call`, for `deferred`'s servers.
pub fn tools(deferred: &Arc<Deferred>) -> Vec<DynamicTool> {
    if deferred.is_empty() {
        return Vec::new();
    }
    let servers: Vec<&str> = deferred
        .servers
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    let search = {
        let deferred = deferred.clone();
        DynamicTool::new(
            name(SEARCH),
            format!(
                "Look up the tools of deferred MCP servers ({}) by words of their names or \
                 descriptions, giving each one's description and parameters. Call one found with \
                 `{CALL}`.",
                servers.join(", ")
            ),
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words to look for; empty lists every tool." },
                    "server": { "type": "string", "enum": servers, "description": "Only this server's tools." },
                },
                "required": ["query"],
            }),
            move |args| {
                let deferred = deferred.clone();
                Box::pin(async move {
                    let query = text(&args, "query").unwrap_or_default();
                    let found: Vec<Value> = deferred
                        .search(&query, text(&args, "server").as_deref())
                        .into_iter()
                        .map(|(server, tool)| {
                            let ToolDefinition { description, parameters, .. } = tool.tool.definition();
                            json!({ "server": server, "tool": mcp_tool(tool), "description": description, "parameters": parameters })
                        })
                        .collect();
                    match found.is_empty() {
                        true => Ok(ToolOutput::text(format!("No tool matches `{query}`."))),
                        false => Ok(ToolOutput::json(Value::Array(found))),
                    }
                })
            },
        )
        .with_annotations(reading())
    };
    let call = {
        let deferred = deferred.clone();
        DynamicTool::new_with_context(
            name(CALL),
            format!(
                "Call a tool of a deferred MCP server, with the parameters `{SEARCH}` gave for it."
            ),
            json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "enum": servers },
                    "tool": { "type": "string", "description": "The tool's name, as the server gives it." },
                    "arguments": { "type": "object", "description": "The tool's arguments." },
                },
                "required": ["server", "tool", "arguments"],
            }),
            move |context, args| {
                let deferred = deferred.clone();
                Box::pin(async move {
                    let (Some(server), Some(tool)) = (text(&args, "server"), text(&args, "tool"))
                    else {
                        return Err(invalid("`server` and `tool` are required"));
                    };
                    let Some(found) = deferred.find(&server, &tool) else {
                        return Err(ToolExecutionError::new(
                            ToolErrorKind::NotFound,
                            format!(
                                "`{server}` has no deferred tool `{tool}`; `{SEARCH}` finds them"
                            ),
                        ));
                    };
                    let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    found.tool.run_with(context, arguments).await
                })
            },
        )
    };
    vec![search, call]
}

/// `mcp_resources` and `mcp_read_resource`, for the servers that list
/// resources.
pub fn resource_tools(servers: &Arc<[(String, McpConnection)]>) -> Vec<DynamicTool> {
    if servers.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
    let list = {
        let servers = servers.clone();
        DynamicTool::new(
            name(RESOURCES),
            "List the resources MCP servers offer to read: data such as files, records or \
             documents, each by its URI.",
            json!({
                "type": "object",
                "properties": { "server": { "type": "string", "enum": names, "description": "Only this server's." } },
            }),
            move |args| {
                let servers = servers.clone();
                Box::pin(async move {
                    let only = text(&args, "server");
                    let mut listed = Vec::new();
                    for (server, connection) in servers.iter().filter(|(name, _)| only.as_ref().is_none_or(|only| only == name)) {
                        match connection.resources().await {
                            Ok(resources) => listed.extend(resources.into_iter().map(|resource| {
                                json!({
                                    "server": server,
                                    "uri": resource.uri,
                                    "name": resource.name,
                                    "description": resource.description,
                                    "mime_type": resource.mime_type,
                                })
                            })),
                            Err(error) => listed.push(json!({ "server": server, "error": error.to_string() })),
                        }
                    }
                    Ok(ToolOutput::json(Value::Array(listed)))
                })
            },
        )
        .with_annotations(reading())
    };
    let read = {
        let servers = servers.clone();
        DynamicTool::new(
            name(READ_RESOURCE),
            format!("Read a resource of an MCP server, by the URI `{RESOURCES}` gave."),
            json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "enum": names },
                    "uri": { "type": "string" },
                },
                "required": ["server", "uri"],
            }),
            move |args| {
                let servers = servers.clone();
                Box::pin(async move {
                    let (Some(server), Some(uri)) = (text(&args, "server"), text(&args, "uri"))
                    else {
                        return Err(invalid("`server` and `uri` are required"));
                    };
                    let Some((_, connection)) = servers.iter().find(|(name, _)| *name == server)
                    else {
                        return Err(invalid(format!("no server `{server}` with resources")));
                    };
                    let contents = connection.read_resource(uri).await.map_err(|error| {
                        ToolExecutionError::new(ToolErrorKind::Other, error.to_string())
                    })?;
                    let texts: Vec<String> = contents
                        .into_iter()
                        .map(|content| match content {
                            ResourceContents::TextResourceContents { text, .. } => text,
                            ResourceContents::BlobResourceContents {
                                uri,
                                mime_type,
                                blob,
                                ..
                            } => format!(
                                "[{uri}: {} bytes of {}, not text]",
                                blob.len() / 4 * 3,
                                mime_type.as_deref().unwrap_or("binary data")
                            ),
                            _ => "[contents of a kind hey cannot read]".into(),
                        })
                        .collect();
                    Ok(ToolOutput::text(texts.join("\n")))
                })
            },
        )
        .with_annotations(reading())
    };
    vec![list, read]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Origin;

    fn tool(server: &str, tool: &str, description: &str) -> Tool {
        let name = model_name(server, tool);
        Tool {
            tool: DynamicTool::new(
                ToolName::new(name.clone()).unwrap(),
                description,
                json!({ "type": "object" }),
                |args| Box::pin(async move { Ok(ToolOutput::json(args)) }),
            ),
            origin: Origin::Mcp {
                server: server.into(),
                tool: tool.into(),
            },
            name,
        }
    }

    fn deferred() -> Arc<Deferred> {
        Arc::new(Deferred::new(vec![Server {
            name: "web".into(),
            summary: Some("Scrapes pages.".into()),
            tools: vec![
                tool("web", "scrape", "Scrape one page as Markdown"),
                tool("web", "crawl", "Crawl a site, page after page"),
            ],
        }]))
    }

    #[tokio::test]
    async fn search_finds_tools_and_call_runs_them() {
        let deferred = deferred();
        let preamble = deferred.preamble().unwrap();
        assert!(
            preamble.contains("`web`: Scrapes pages. Tools: scrape, crawl."),
            "{preamble}"
        );
        let found = deferred.search("page markdown", None);
        assert_eq!(
            found
                .iter()
                .map(|(_, tool)| mcp_tool(tool))
                .collect::<Vec<_>>(),
            ["scrape", "crawl"]
        );
        assert!(deferred.search("nothing", None).is_empty());
        assert!(deferred.search("", Some("other")).is_empty());

        let [search, call] = <[DynamicTool; 2]>::try_from(tools(&deferred)).ok().unwrap();
        let output = search.run(json!({ "query": "crawl" })).await.unwrap();
        assert_eq!(output.as_json().unwrap()[0]["tool"], "crawl");
        let output = call
            .run(json!({ "server": "web", "tool": "scrape", "arguments": { "url": "u" } }))
            .await
            .unwrap();
        assert_eq!(output.as_json(), Some(&json!({ "url": "u" })));
        assert!(
            call.run(json!({ "server": "web", "tool": "nope", "arguments": {} }))
                .await
                .is_err()
        );
    }

    #[test]
    fn a_call_is_permitted_as_the_tool_it_calls() {
        let args = r#"{"server": "web", "tool": "scrape", "arguments": {}}"#;
        assert_eq!(called(CALL, args).as_deref(), Some("mcp__web__scrape"));
        assert_eq!(called("read", args), None);
        assert_eq!(called(CALL, "{}"), None);
    }
}
