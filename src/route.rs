/* -------------------------------------------------------------------------- */
/*                                  route.rs                                  */
/* -------------------------------------------------------------------------- */

//! Which machine each of an agent's tools runs on: the host, or the remote
//! client following the run, which lends its own tools. A tool both offer
//! takes a `machine` argument, which every call to it names.

use std::collections::HashMap;
use std::sync::Arc;

use rig_core::completion::ToolDefinition;
use rig_core::tool::{DynamicTool, ToolErrorKind, ToolExecutionError, ToolOutput};
use rig_tools::{Live, Seen};
use serde_json::{Value, json};

use crate::host::Run;
use crate::protocol::{AgentId, Call, Lent};
use crate::toolbox::{Place, Toolbox};

/// The argument that names the machine a call runs on.
pub const ARG: &str = "machine";

/// What a run's agents can call: this host's tools, and those the client
/// that started the run lends.
#[derive(Clone)]
pub struct Kit {
    pub toolbox: Arc<Toolbox>,
    /// This host's name.
    pub host: String,
    pub lent: Option<Lent>,
    pub run: Arc<Run>,
}

/// A subagent, which the calls it makes on the client are shown under.
#[derive(Clone)]
pub struct Tag {
    pub agent: AgentId,
    pub label: String,
}

impl Kit {
    /// The tools of one agent, sharing what it has `seen` and showing a
    /// running command's output through `live`; `tag` names a subagent.
    pub fn tools(&self, seen: &Seen, live: Option<Live>, tag: Option<Tag>) -> (Vec<DynamicTool>, Arc<Routes>) {
        let own = self.toolbox.tools(seen, Place::here(live));
        let Some(lent) = &self.lent else {
            return (own, Arc::new(Routes::default()));
        };
        let routes = Arc::new(Routes::new(&self.host, lent, &own));
        let mut tools: Vec<DynamicTool> = own
            .into_iter()
            .map(|tool| match routes.on.get(tool.name().as_str()) {
                Some(On::Either) => self.either(tool, &routes, tag.clone()),
                _ => tool,
            })
            .collect();
        let client_only = lent
            .tools
            .iter()
            .filter(|tool| routes.on.get(tool.name.as_str()) == Some(&On::Client))
            .map(|tool| self.client_only(tool.clone(), &lent.machine, tag.clone()));
        tools.extend(client_only);
        (tools, routes)
    }

    /// `tool`, run on the host or the client as the call's `machine` says.
    fn either(&self, tool: DynamicTool, routes: &Arc<Routes>, tag: Option<Tag>) -> DynamicTool {
        let definition = tool.definition();
        let (host, client) = (&self.host, routes.client.as_deref().unwrap_or_default());
        let description = format!(
            "{}\n\nRuns on the machine `{ARG}` names: `{host}`, where you run, or `{client}`, the user's machine.",
            definition.description
        );
        let parameters = with_machine(definition.parameters, host, client);
        let (run, routes) = (self.run.clone(), routes.clone());
        let name = definition.name.clone();
        DynamicTool::new_with_context(definition.name, description, parameters, move |context, mut args| {
            let (tool, run, routes, name, tag) = (tool.clone(), run.clone(), routes.clone(), name.clone(), tag.clone());
            Box::pin(async move {
                let machine = take_machine(&mut args);
                match routes.target(name.as_str(), machine.as_deref()) {
                    Ok(Target::Host) => tool.run_with(context, args).await,
                    Ok(Target::Client(machine)) => call(&run, machine, name.as_str(), args, tag).await,
                    Err(error) => Err(ToolExecutionError::invalid_args(error)),
                }
            })
        })
        .with_annotations(definition.annotations)
    }

    /// A tool only the client has, run there.
    fn client_only(&self, definition: ToolDefinition, machine: &str, tag: Option<Tag>) -> DynamicTool {
        let description = format!("{}\n\nRuns on `{machine}`, the user's machine.", definition.description);
        let (run, machine) = (self.run.clone(), machine.to_owned());
        let name = definition.name.clone();
        DynamicTool::new_with_context(definition.name, description, definition.parameters, move |_context, args| {
            let (run, machine, name, tag) = (run.clone(), machine.clone(), name.clone(), tag.clone());
            Box::pin(async move { call(&run, machine, name.as_str(), args, tag).await })
        })
        .with_annotations(definition.annotations)
    }
}

/// Has the client on `machine` run `tool`, through `run`.
async fn call(
    run: &Run,
    machine: String,
    tool: &str,
    args: Value,
    tag: Option<Tag>,
) -> Result<ToolOutput, ToolExecutionError> {
    let call = Call {
        machine,
        tool: tool.to_owned(),
        args: args.to_string(),
        agent: tag.as_ref().map(|tag| tag.label.clone()),
    };
    let content = run
        .call(call, tag.map(|tag| tag.agent))
        .await
        .map_err(|error| ToolExecutionError::new(ToolErrorKind::Other, error))?;
    match content.is_empty() {
        true => Ok(ToolOutput::text("")),
        false => ToolOutput::content(content),
    }
}

/// Where a call runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Host,
    Client(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    Client,
    /// The host, or the client when the call names it.
    Either,
}

/// Which of an agent's tools run on the client, alone or as the call says.
/// Every other runs on the host.
#[derive(Debug, Default)]
pub struct Routes {
    host: String,
    client: Option<String>,
    on: HashMap<String, On>,
}

impl Routes {
    fn new(host: &str, lent: &Lent, own: &[DynamicTool]) -> Self {
        let on = lent
            .tools
            .iter()
            .filter_map(|tool| {
                let shared = own.iter().find(|own| own.name() == &tool.name);
                let on = match shared {
                    None => On::Client,
                    // A tool with an argument of the same name stays the host's.
                    Some(own) if own.definition().parameters["properties"].get(ARG).is_some() => return None,
                    Some(_) => On::Either,
                };
                Some((tool.name.as_str().to_owned(), on))
            })
            .collect();
        Self {
            host: host.to_owned(),
            client: Some(lent.machine.clone()),
            on,
        }
    }

    /// Where a call to `tool` with `args` runs.
    pub fn target_of(&self, tool: &str, args: &str) -> Result<Target, String> {
        let args: Value = serde_json::from_str(args).unwrap_or_default();
        self.target(tool, args.get(ARG).and_then(Value::as_str))
    }

    /// Where a call to `tool` naming `machine` runs. A call to a tool both
    /// have must name one: taking the host for a call that left it out sent
    /// calls meant for the user's machine, such as writes to their paths,
    /// to the host, where they failed in ways that did not say why.
    fn target(&self, tool: &str, machine: Option<&str>) -> Result<Target, String> {
        let client = || self.client.clone().unwrap_or_default();
        match (self.on.get(tool), machine) {
            (None, _) => Ok(Target::Host),
            (Some(On::Client), _) => Ok(Target::Client(client())),
            (Some(On::Either), Some(machine)) if machine == self.host => Ok(Target::Host),
            (Some(On::Either), Some(machine)) if Some(machine) == self.client.as_deref() => {
                Ok(Target::Client(client()))
            }
            (Some(On::Either), None) => Err(format!(
                "`{ARG}` is required: `{}`, where you run, or `{}`, the user's machine",
                self.host,
                client()
            )),
            (Some(On::Either), Some(machine)) => Err(format!(
                "no machine `{machine}`; `{ARG}` is `{}` or `{}`",
                self.host,
                client()
            )),
        }
    }
}

/// `parameters` with the required `machine` argument, which names `host` or
/// `client`.
fn with_machine(mut parameters: Value, host: &str, client: &str) -> Value {
    let argument = json!({
        "type": "string",
        "enum": [host, client],
        "description": format!(
            "The machine it runs on: `{host}`, where you run, or `{client}`, the user's machine."
        ),
    });
    if let Some(schema) = parameters.as_object_mut() {
        let properties = schema.entry("properties").or_insert_with(|| json!({}));
        if let Some(properties) = properties.as_object_mut() {
            properties.insert(ARG.to_owned(), argument);
        }
        let required = schema.entry("required").or_insert_with(|| json!([]));
        if let Some(required) = required.as_array_mut() {
            required.push(json!(ARG));
        }
    }
    parameters
}

/// Takes the `machine` argument out of `args`, which the tool itself does
/// not take.
fn take_machine(args: &mut Value) -> Option<String> {
    let machine = args.as_object_mut()?.remove(ARG)?;
    machine.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use rig_core::message::ToolName;

    use super::*;

    fn tool(name: &str, parameters: Value) -> DynamicTool {
        DynamicTool::new(ToolName::new(name).unwrap(), "", parameters, |_| {
            Box::pin(async { Ok(ToolOutput::text("")) })
        })
    }

    fn routes() -> Routes {
        let own = [
            tool("read", json!({ "type": "object", "properties": { "path": {} } })),
            tool("skill", json!({ "type": "object" })),
            tool("mcp__x__y", json!({ "type": "object", "properties": { "machine": {} } })),
        ];
        let lent = Lent {
            machine: "laptop".into(),
            tools: ["read", "mcp__nu__run", "mcp__x__y"]
                .map(|name| tool(name, json!({ "type": "object" })).definition())
                .into(),
        };
        Routes::new("fermi", &lent, &own)
    }

    #[test]
    fn calls_to_a_tool_both_have_name_the_machine_and_others_run_where_the_tool_is() {
        let routes = routes();
        let laptop = || Ok(Target::Client("laptop".into()));
        assert!(routes.target_of("read", r#"{"path": "a"}"#).unwrap_err().contains("`machine` is required"));
        assert_eq!(routes.target_of("read", r#"{"machine": "fermi"}"#), Ok(Target::Host));
        assert_eq!(routes.target_of("read", r#"{"machine": "laptop"}"#), laptop());
        assert!(routes.target_of("read", r#"{"machine": "desk"}"#).unwrap_err().contains("no machine `desk`"));
        assert_eq!(routes.target_of("skill", r#"{"machine": "laptop"}"#), Ok(Target::Host), "the host's own");
        assert_eq!(routes.target_of("mcp__nu__run", "{}"), laptop(), "the client's own");
        assert_eq!(
            routes.target_of("mcp__x__y", r#"{"machine": "laptop"}"#),
            Ok(Target::Host),
            "a tool with its own `machine` argument stays the host's"
        );
    }

    #[test]
    fn the_machine_argument_is_required_and_taken_out_before_the_tool_sees_it() {
        let parameters = with_machine(json!({ "type": "object", "required": ["path"] }), "fermi", "laptop");
        assert_eq!(parameters["properties"]["machine"]["enum"], json!(["fermi", "laptop"]));
        assert_eq!(parameters["required"], json!(["path", "machine"]));

        let mut args = json!({ "path": "a", "machine": "laptop" });
        assert_eq!(take_machine(&mut args).as_deref(), Some("laptop"));
        assert_eq!(args, json!({ "path": "a" }));
    }
}
