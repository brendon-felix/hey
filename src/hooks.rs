/* -------------------------------------------------------------------------- */
/*                                  hooks.rs                                  */
/* -------------------------------------------------------------------------- */

//! Hooks: commands the config runs at points of a conversation, which can
//! block what happens there or add to what the model reads. Each runs with
//! `sh -c` on the host, given what happened as JSON on its stdin.

use std::collections::{BTreeMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rig_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
};
use rig_core::effect::EffectId;
use rig_core::message::ToolResultContent;
use rig_core::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::host::Run;
use crate::protocol::Event;

/// Most times in a row a `Stop` hook may send the agent back to work.
pub const MAX_STOP_BLOCKS: usize = 8;

/// Where in a conversation a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
    SubagentStop,
}

/// A command run at an event, as the config gives it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookCommand {
    pub command: String,
    /// For `PreToolUse` and `PostToolUse`, the tools it runs for: a regex
    /// matching the whole name. All of them when unset.
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    60
}

/// The hooks of every event, as configured.
pub type Config = BTreeMap<HookEvent, Vec<HookCommand>>;

struct Entry {
    command: String,
    matcher: Option<Regex>,
    timeout: Duration,
}

/// The config's hooks, their matchers compiled.
#[derive(Default)]
pub struct Hooks(BTreeMap<HookEvent, Vec<Entry>>);

impl Hooks {
    pub fn new(config: &Config) -> Result<Self> {
        let mut hooks = BTreeMap::new();
        for (event, commands) in config {
            let entries = commands
                .iter()
                .map(|hook| {
                    let matcher = hook
                        .matcher
                        .as_deref()
                        .map(|matcher| Regex::new(&format!("^(?:{matcher})$")))
                        .transpose()
                        .with_context(|| format!("invalid matcher in a `{event:?}` hook"))?;
                    Ok(Entry {
                        command: hook.command.clone(),
                        matcher,
                        timeout: Duration::from_secs(hook.timeout_secs),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            hooks.insert(*event, entries);
        }
        Ok(Self(hooks))
    }

    pub fn has(&self, event: HookEvent) -> bool {
        self.0
            .get(&event)
            .is_some_and(|entries| !entries.is_empty())
    }
}

/// What the hooks run at an event decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Why one blocked what happens, if one did.
    pub block: Option<String>,
    /// What they add for the model to read.
    pub context: Vec<String>,
    /// Why any failed; a failed hook blocks nothing.
    pub failures: Vec<String>,
}

impl Verdict {
    /// Tells `run`'s clients why any hook failed.
    pub fn tell(&self, run: &Run) {
        for failure in &self.failures {
            run.emit(Event::Notice {
                text: failure.clone(),
            });
        }
    }

    /// What was added, as one text.
    pub fn context(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n\n"))
    }
}

/// The hooks of one conversation, which every payload names.
#[derive(Clone)]
pub struct Scope {
    pub hooks: Arc<Hooks>,
    pub conversation: String,
}

impl Scope {
    pub fn has(&self, event: HookEvent) -> bool {
        self.hooks.has(event)
    }

    /// Runs `event`'s hooks, for `tool` when one is called, one after
    /// another, each given the event, conversation, working directory and
    /// `fields` on stdin.
    pub async fn fire(&self, event: HookEvent, tool: Option<&str>, fields: Value) -> Verdict {
        let mut verdict = Verdict::default();
        let Some(entries) = self.hooks.0.get(&event) else {
            return verdict;
        };
        let mut payload = json!({
            "event": format!("{event:?}"),
            "conversation_id": self.conversation,
            "cwd": crate::agent::working_directory(),
        });
        if let (Some(payload), Value::Object(fields)) = (payload.as_object_mut(), fields) {
            payload.extend(fields);
        }
        let payload = payload.to_string();
        let runs = entries.iter().filter(|entry| match (&entry.matcher, tool) {
            (Some(matcher), Some(tool)) => matcher.is_match(tool),
            _ => true,
        });
        let mut blocks = Vec::new();
        for entry in runs {
            match execute(entry, &payload).await {
                Ok(Reply { block, context }) => {
                    blocks.extend(block);
                    verdict.context.extend(context);
                }
                Err(error) => {
                    tracing::warn!(
                        ?event,
                        command = entry.command,
                        error = format!("{error:#}"),
                        "hook failed"
                    );
                    verdict
                        .failures
                        .push(format!("`{event:?}` hook failed: {error:#}"));
                }
            }
        }
        verdict.block = (!blocks.is_empty()).then(|| blocks.join("\n"));
        verdict
    }
}

/// What one hook said.
struct Reply {
    block: Option<String>,
    context: Option<String>,
}

#[derive(Deserialize)]
struct Said {
    decision: Option<String>,
    reason: Option<String>,
    additional_context: Option<String>,
}

const BLOCKED: &str = "blocked by a hook";

async fn execute(entry: &Entry, payload: &str) -> Result<Reply> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&entry.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("cannot start `sh`")?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_owned();
        // A hook may not read its stdin, so this is not waited for.
        tokio::spawn(async move {
            _ = stdin.write_all(payload.as_bytes()).await;
        });
    }
    let output = tokio::time::timeout(entry.timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "`{}` timed out after {}s",
                entry.command,
                entry.timeout.as_secs()
            )
        })??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(read(stdout.trim())),
        Some(2) => Ok(Reply {
            block: Some(
                Some(stderr.trim())
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or(BLOCKED)
                    .to_owned(),
            ),
            context: None,
        }),
        code => {
            let code = code.map_or("a signal".to_owned(), |code| code.to_string());
            Err(anyhow!(
                "`{}` exited with {code}: {}",
                entry.command,
                stderr.trim()
            ))
        }
    }
}

/// What a hook that succeeded wrote: JSON deciding or adding something, or
/// anything else, which says nothing.
fn read(stdout: &str) -> Reply {
    let said = stdout
        .starts_with('{')
        .then(|| serde_json::from_str::<Said>(stdout).ok())
        .flatten();
    let Some(said) = said else {
        return Reply {
            block: None,
            context: None,
        };
    };
    let blocks = said.decision.as_deref() == Some("block");
    Reply {
        block: blocks.then(|| said.reason.unwrap_or_else(|| BLOCKED.to_owned())),
        context: said
            .additional_context
            .filter(|context| !context.trim().is_empty()),
    }
}

/// Runs the `PreToolUse` and `PostToolUse` hooks around the agent's tool
/// calls. It goes before the hook that asks for permission, so a call a
/// hook blocks is never asked about.
pub struct ToolHooks {
    scope: Scope,
    run: Arc<Run>,
    /// The calls a hook blocked, which `PostToolUse` hooks skip.
    blocked: Mutex<HashSet<EffectId>>,
}

impl ToolHooks {
    pub fn new(scope: Scope, run: Arc<Run>) -> Self {
        Self {
            scope,
            run,
            blocked: Mutex::default(),
        }
    }
}

impl AgentHook for ToolHooks {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let Some(name) = event
            .tool_name()
            .filter(|_| self.scope.has(HookEvent::PreToolUse))
        else {
            return DispatchAction::proceed();
        };
        let fields = json!({ "tool_name": name, "tool_input": args(event.tool_args()) });
        let verdict = self
            .scope
            .fire(HookEvent::PreToolUse, Some(name), fields)
            .await;
        verdict.tell(&self.run);
        match verdict.block {
            Some(reason) => {
                self.blocked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(event.id);
                self.run.emit(Event::Notice {
                    text: format!("a hook blocked `{name}`: {reason}"),
                });
                DispatchAction::skip(format!("A hook blocked this call: {reason}"))
            }
            None => DispatchAction::proceed(),
        }
    }

    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        let (Some(name), Some(result)) = (event.tool_name(), event.tool_result()) else {
            return OutcomeAction::proceed();
        };
        let blocked = self
            .blocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&event.id);
        if blocked || !self.scope.has(HookEvent::PostToolUse) {
            return OutcomeAction::proceed();
        }
        let blocks = result.output().as_content();
        let response: Vec<&str> = blocks
            .iter()
            .filter_map(ToolResultContent::as_text)
            .collect();
        let fields = json!({
            "tool_name": name,
            "tool_input": args(event.tool_args()),
            "tool_response": response.join("\n"),
            "is_error": result.error().is_some(),
        });
        let verdict = self
            .scope
            .fire(HookEvent::PostToolUse, Some(name), fields)
            .await;
        verdict.tell(&self.run);
        let added: Vec<String> = verdict.block.into_iter().chain(verdict.context).collect();
        if added.is_empty() {
            return OutcomeAction::proceed();
        }
        let content = blocks
            .iter()
            .cloned()
            .chain(added.into_iter().map(ToolResultContent::text));
        match ToolOutput::content(content.collect()) {
            Ok(output) => OutcomeAction::rewrite_tool_output(&event, output),
            Err(_) => OutcomeAction::proceed(),
        }
    }
}

fn args(args: Option<&str>) -> Value {
    let args = args.unwrap_or_default();
    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(config: &str) -> Scope {
        let config: BTreeMap<String, Config> = toml::from_str(config).unwrap();
        Scope {
            hooks: Arc::new(Hooks::new(&config["hooks"]).unwrap()),
            conversation: "c1".into(),
        }
    }

    #[tokio::test]
    async fn hooks_block_add_context_and_fail_by_their_exit() {
        let scope = scope(
            r#"
            [[hooks.PreToolUse]]
            matcher = "shell|write"
            command = "grep -q '\"tool_name\":\"shell\"' && echo 'no shells' >&2 && exit 2; exit 0"

            [[hooks.PreToolUse]]
            command = "echo '{\"additional_context\": \"seen\"}'"

            [[hooks.Stop]]
            command = "cat >/dev/null; echo '{\"decision\": \"block\", \"reason\": \"tests fail\"}'"

            [[hooks.PreCompact]]
            command = "exit 3"

            [[hooks.SessionStart]]
            command = "sleep 5"
            timeout_secs = 1
            "#,
        );
        let shell = scope
            .fire(
                HookEvent::PreToolUse,
                Some("shell"),
                json!({ "tool_name": "shell" }),
            )
            .await;
        assert_eq!(shell.block.as_deref(), Some("no shells"));
        assert_eq!(shell.context, ["seen"]);
        let read = scope
            .fire(
                HookEvent::PreToolUse,
                Some("read"),
                json!({ "tool_name": "read" }),
            )
            .await;
        assert_eq!(read.block, None, "the matcher matches whole names");

        let stop = scope.fire(HookEvent::Stop, None, json!({})).await;
        assert_eq!(stop.block.as_deref(), Some("tests fail"));
        let compact = scope.fire(HookEvent::PreCompact, None, json!({})).await;
        assert_eq!(compact.block, None);
        assert!(
            compact.failures[0].contains("exited with 3"),
            "{:?}",
            compact.failures
        );
        let start = scope.fire(HookEvent::SessionStart, None, json!({})).await;
        assert!(
            start.failures[0].contains("timed out"),
            "{:?}",
            start.failures
        );
        assert_eq!(
            scope.fire(HookEvent::SubagentStop, None, json!({})).await,
            Verdict::default()
        );
    }

    #[tokio::test]
    async fn the_payload_names_the_event_and_conversation() {
        let scope = scope(
            r#"
            [[hooks.UserPromptSubmit]]
            command = "p=$(cat); echo \"$p\" | grep -q '\"event\":\"UserPromptSubmit\"' && echo \"$p\" | grep -q '\"conversation_id\":\"c1\"' && echo '{\"additional_context\": \"named\"}'"
            "#,
        );
        let verdict = scope
            .fire(HookEvent::UserPromptSubmit, None, json!({ "prompt": "hi" }))
            .await;
        assert_eq!(verdict.context, ["named"], "{verdict:?}");
    }

    #[tokio::test]
    async fn a_blocked_call_never_runs_and_a_result_gets_what_hooks_add() {
        use rig_agent::agent::AgentBuilder;
        use rig_core::test_utils::{MockCompletionModel, MockTurn};

        let scope = scope(
            r#"
            [[hooks.PreToolUse]]
            command = "grep -q '\"name\":\"blocked\"' && echo '{\"decision\": \"block\", \"reason\": \"not that one\"}'; exit 0"

            [[hooks.PostToolUse]]
            matcher = "skill"
            command = "echo '{\"additional_context\": \"checked by a hook\"}'"
            "#,
        );
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "skill", json!({ "name": "blocked" })),
            MockTurn::tool_call("2", "skill", json!({ "name": "other" })),
            MockTurn::text("done"),
        ]);
        let run = Arc::new(Run::new("q".into(), "openai/mock".into(), 0, || {}));
        AgentBuilder::new(model.clone())
            .tool(crate::skills::Load {
                skills: Arc::from([]),
            })
            .add_hook(ToolHooks::new(scope, run))
            .default_max_turns(4)
            .build()
            .prompt("q")
            .run()
            .await
            .unwrap();
        let sent = format!("{:?}", model.requests().last().unwrap());
        assert!(
            sent.contains("A hook blocked this call: not that one"),
            "{sent}"
        );
        assert_eq!(
            sent.matches("checked by a hook").count(),
            1,
            "only the call that ran: {sent}"
        );
    }

    #[test]
    fn a_bad_matcher_is_reported() {
        let config: Config =
            toml::from_str("[[PreToolUse]]\ncommand = \"true\"\nmatcher = \"(\"").unwrap();
        assert!(Hooks::new(&config).is_err());
        assert!(toml::from_str::<Config>("[[OnBoot]]\ncommand = \"true\"").is_err());
    }
}
