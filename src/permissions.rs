/* -------------------------------------------------------------------------- */
/*                               permissions.rs                               */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use rig_agent::agent::{AgentHook, DispatchAction, DispatchEvent, HookContext};
use serde::Deserialize;

use crate::{render, ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Confirm,
    Deny,
}

pub fn default_rules() -> BTreeMap<String, Permission> {
    ["read", "list", "search"]
        .map(|tool| (tool.to_owned(), Permission::Allow))
        .into()
}

/// Where a tool the model can call comes from, which decides the key its
/// permission is configured under.
#[derive(Debug, Clone)]
pub enum Origin {
    Builtin,
    Mcp { server: String, tool: String },
}

impl Origin {
    /// The config keys that apply, most specific first.
    fn keys(&self, name: &str) -> Vec<String> {
        match self {
            Origin::Builtin => vec![name.to_owned()],
            Origin::Mcp { server, tool } => vec![format!("mcp:{server}:{tool}"), format!("mcp:{server}")],
        }
    }
}

/// The permission of every tool the agent offers, by the name the model
/// calls it. Shared between the approval hook and the REPL.
#[derive(Debug, Default)]
pub struct Permissions(Mutex<BTreeMap<String, Permission>>);

impl Permissions {
    pub fn resolve<'a>(
        tools: impl IntoIterator<Item = (&'a str, &'a Origin)>,
        config: &crate::config::Tools,
    ) -> Self {
        let resolved = tools
            .into_iter()
            .map(|(name, origin)| {
                let permission = origin
                    .keys(name)
                    .iter()
                    .find_map(|key| config.permissions.get(key))
                    .copied()
                    .unwrap_or(config.default);
                (name.to_owned(), permission)
            })
            .collect();
        Self(Mutex::new(resolved))
    }

    pub fn get(&self, tool: &str) -> Permission {
        self.lock().get(tool).copied().unwrap_or(Permission::Confirm)
    }

    pub fn snapshot(&self) -> BTreeMap<String, Permission> {
        self.lock().clone()
    }

    fn allow(&self, tool: &str) {
        self.lock().insert(tool.to_owned(), Permission::Allow);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Permission>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Shows every tool call and gates it on its permission.
pub struct Approval(pub Arc<Permissions>);

impl AgentHook for Approval {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let Some(name) = event.tool_name() else {
            return DispatchAction::proceed();
        };
        render::tool_call(name, event.tool_args().unwrap_or_default());
        match self.0.get(name) {
            Permission::Allow => DispatchAction::proceed(),
            Permission::Deny => {
                render::notice("denied by config");
                DispatchAction::skip(format!("The user's configuration does not allow `{name}`."))
            }
            Permission::Confirm => match ui::confirm_tool(name.to_owned()).await {
                ui::Approval::Once => DispatchAction::proceed(),
                ui::Approval::Always => {
                    self.0.allow(name);
                    DispatchAction::proceed()
                }
                ui::Approval::Decline => {
                    DispatchAction::skip("The user declined this tool call.")
                }
                ui::Approval::Stop => DispatchAction::stop("The user stopped the run."),
            },
        }
    }
}
