/* -------------------------------------------------------------------------- */
/*                               permissions.rs                               */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use rig_agent::agent::{AgentHook, DispatchAction, DispatchEvent, HookContext};
use serde::{Deserialize, Serialize};

use crate::host::Run;
use crate::protocol::{Ask, Decision, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Confirm,
    Deny,
}

pub fn default_rules() -> BTreeMap<String, Permission> {
    // Starting a subagent asks for its directory, and its tools ask as
    // their own rules say, so the call itself need not ask too.
    ["read", "list", "search", crate::subagents::TOOL]
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
/// calls it. Each thread has its own, as "always allow" applies to one thread.
#[derive(Debug, Default)]
pub struct Permissions(Mutex<BTreeMap<String, Permission>>);

impl Permissions {
    pub fn resolve<'a>(
        tools: impl IntoIterator<Item = (&'a str, &'a Origin)>,
        config: &crate::config::Tools,
    ) -> Self {
        let permission = |name: &str, origin: &Origin| {
            if config.allow_all {
                return Permission::Allow;
            }
            origin
                .keys(name)
                .iter()
                .find_map(|key| config.permissions.get(key))
                .copied()
                .unwrap_or(config.default)
        };
        let resolved = tools
            .into_iter()
            .map(|(name, origin)| (name.to_owned(), permission(name, origin)))
            .collect();
        Self(Mutex::new(resolved))
    }

    pub fn get(&self, tool: &str) -> Permission {
        self.lock().get(tool).copied().unwrap_or(Permission::Confirm)
    }

    pub fn snapshot(&self) -> BTreeMap<String, Permission> {
        self.lock().clone()
    }

    pub fn fork(&self) -> Self {
        Self(Mutex::new(self.snapshot()))
    }

    fn allow(&self, tool: &str) {
        self.lock().insert(tool.to_owned(), Permission::Allow);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Permission>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where an agent's tool calls are shown and its questions asked: the run
/// it is part of, on this host or a remote one.
pub trait Reporter: Send + Sync + 'static {
    fn emit(&self, event: Event) -> impl Future<Output = ()> + Send;

    /// Waits for a client to decide `ask`.
    fn ask(&self, ask: Ask) -> impl Future<Output = Decision> + Send;

    /// Stops the whole run, as Ctrl+C at a question does.
    fn stop(&self) -> impl Future<Output = ()> + Send;
}

impl Reporter for Run {
    async fn emit(&self, event: Event) {
        Run::emit(self, event);
    }

    async fn ask(&self, ask: Ask) -> Decision {
        Run::ask(self, ask).await
    }

    async fn stop(&self) {
        Run::stop(self);
    }
}

/// Reports every tool call and gates it on its permission, asking the
/// run's clients when it needs confirming.
pub struct Approval<R> {
    pub permissions: Arc<Permissions>,
    pub reporter: Arc<R>,
}

impl<R: Reporter> AgentHook for Approval<R> {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let Some(name) = event.tool_name() else {
            return DispatchAction::proceed();
        };
        let call = Event::ToolCall {
            name: name.to_owned(),
            args: event.tool_args().unwrap_or_default().to_owned(),
        };
        self.reporter.emit(call).await;
        match self.permissions.get(name) {
            Permission::Allow => DispatchAction::proceed(),
            Permission::Deny => {
                let notice = Event::Notice {
                    text: "denied by config".into(),
                };
                self.reporter.emit(notice).await;
                DispatchAction::skip(format!("The user's configuration does not allow `{name}`."))
            }
            Permission::Confirm => {
                let ask = Ask::Tool {
                    name: name.to_owned(),
                    agent: None,
                };
                match self.reporter.ask(ask).await {
                    Decision::Once => DispatchAction::proceed(),
                    Decision::Always => {
                        self.permissions.allow(name);
                        DispatchAction::proceed()
                    }
                    Decision::Decline => DispatchAction::skip("The user declined this tool call."),
                    Decision::Stop => DispatchAction::stop("The user stopped the run."),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tools;

    fn resolve(config: &str) -> Permissions {
        let config: Tools = toml::from_str(config).unwrap();
        let search = Origin::Mcp {
            server: "web".into(),
            tool: "search".into(),
        };
        let fetch = Origin::Mcp {
            server: "web".into(),
            tool: "fetch".into(),
        };
        Permissions::resolve(
            [
                ("read", &Origin::Builtin),
                ("shell", &Origin::Builtin),
                ("mcp__web__search", &search),
                ("mcp__web__fetch", &fetch),
            ],
            &config,
        )
    }

    #[test]
    fn exact_rules_win_then_server_rules_then_the_default() {
        let permissions = resolve(
            r#"
            default = "deny"
            [permissions]
            "mcp:web" = "confirm"
            "mcp:web:search" = "allow"
            "#,
        );
        assert_eq!(permissions.get("read"), Permission::Allow);
        assert_eq!(permissions.get("shell"), Permission::Deny);
        assert_eq!(permissions.get("mcp__web__search"), Permission::Allow);
        assert_eq!(permissions.get("mcp__web__fetch"), Permission::Confirm);
    }

    #[test]
    fn allow_all_overrides_every_rule() {
        let permissions = resolve(
            r#"
            allow_all = true
            default = "deny"
            [permissions]
            shell = "deny"
            "mcp:web" = "confirm"
            "#,
        );
        for tool in ["read", "shell", "mcp__web__search", "mcp__web__fetch"] {
            assert_eq!(permissions.get(tool), Permission::Allow, "{tool}");
        }
    }
}
