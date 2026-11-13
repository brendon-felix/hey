/* -------------------------------------------------------------------------- */
/*                             workbench/mod.rs                               */
/* -------------------------------------------------------------------------- */

//! What a remote client lends the host's agents: its own tools, run on its
//! machine under its own permissions, in the directories its config and its
//! user allow.

mod access;
mod fence;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use rig_core::tool::{Tool, ToolOutput};
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Delete, Edit, List, Live, Move, Read, Search, Seen, Shell, Write};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent;
use crate::client::Client;
use crate::config::Config;
use crate::permissions::{Permission, Permissions, Reporter};
use crate::protocol::{Ask, Call, CallId, CallResult, Decision, Event, FollowerId, Lent};
use crate::sessions::SessionId;
use crate::toolbox::{Place, Toolbox, machine_name};

use access::{Access, Verdict};
use fence::Fenced;

pub struct Workbench {
    toolbox: Toolbox,
    access: Access,
    /// The permissions of a conversation the host's agents have not called
    /// on here yet.
    permissions: Permissions,
    /// This process's working directory, canonical: where relative paths
    /// start, and what the user is asked to allow for paths inside it.
    cwd: PathBuf,
    conversations: Mutex<HashMap<SessionId, Arc<Lending>>>,
}

/// What a conversation's calls here are allowed, and what they have seen.
struct Lending {
    permissions: Permissions,
    /// Directories the user allowed for this conversation.
    granted: Mutex<Vec<PathBuf>>,
    /// Held while a directory is admitted, so calls that want the same one
    /// at once ask about it once.
    admitting: tokio::sync::Mutex<()>,
    seen: Seen,
}

impl Workbench {
    /// This machine's tools as `config` gives them, its MCP servers connected.
    pub async fn new(config: &Config) -> Result<Self> {
        let machine = machine_name();
        let access = Access::new(machine.clone(), config.machines.get(&machine), config.file.clone())?;
        let toolbox = Toolbox::new(config).await?;
        let permissions = Permissions::resolve(toolbox.origins(), &config.tools);
        let cwd = agent::working_directory();
        Ok(Self {
            cwd: cwd.canonicalize().unwrap_or(cwd),
            toolbox,
            access,
            permissions,
            conversations: Mutex::default(),
        })
    }

    pub fn machine(&self) -> &str {
        self.access.machine()
    }

    pub fn lent(&self) -> Lent {
        Lent {
            machine: self.machine().to_owned(),
            tools: self.toolbox.definitions(),
        }
    }

    /// Runs `call` for conversation `thread` once its permission and the
    /// directories it works in allow it, showing its output and asking its
    /// questions through `reporter`.
    pub async fn run(&self, thread: &SessionId, call: &Call, reporter: &impl Reporter) -> CallResult {
        let lending = self.lending(thread);
        self.permit(&lending, call, reporter).await?;
        let args: Value = serde_json::from_str(&call.args)
            .map_err(|error| format!("unreadable arguments: {error}"))?;
        for dir in directories(&call.tool, &args, &self.cwd) {
            self.admit(&lending, dir, reporter).await?;
        }
        let (output, live) = mpsc::unbounded_channel::<String>();
        let place = Place::on(
            Arc::new(Fenced::new(self.access.excluded())),
            Some(Live::new(move |text: &str| _ = output.send(text.to_owned()))),
        );
        let tool = self
            .toolbox
            .tools(&lending.seen, place)
            .into_iter()
            .find(|tool| tool.name().as_str() == call.tool)
            .ok_or_else(|| format!("`{}` has no tool `{}`", self.machine(), call.tool))?;
        // The tool holds what sends its output, so the output ends with it.
        let running = async move { tool.run(args).await };
        let shown = forward(live, reporter);
        let (ran, ()) = tokio::join!(running, shown);
        ran.map(ToolOutput::into_content)
            .map_err(|error| error.to_string())
    }

    async fn permit(&self, lending: &Lending, call: &Call, reporter: &impl Reporter) -> Result<(), String> {
        let machine = self.machine();
        match lending.permissions.get(&call.tool) {
            Permission::Allow => Ok(()),
            Permission::Deny => Err(format!(
                "The user's configuration on `{machine}` does not allow `{}`.",
                call.tool
            )),
            Permission::Confirm => {
                let ask = Ask::Tool {
                    name: call.tool.clone(),
                    agent: call.agent.clone(),
                    machine: Some(machine.to_owned()),
                };
                match reporter.ask(ask).await {
                    Decision::Once => Ok(()),
                    Decision::Always => {
                        lending.permissions.allow(&call.tool);
                        Ok(())
                    }
                    Decision::Decline => Err("The user declined this tool call.".into()),
                    Decision::Stop => {
                        reporter.stop().await;
                        Err("The user stopped the run.".into())
                    }
                }
            }
        }
    }

    /// Lets a call work in `dir`, a canonical directory, once its rules or
    /// the user allow it.
    async fn admit(&self, lending: &Lending, dir: PathBuf, reporter: &impl Reporter) -> Result<(), String> {
        let machine = self.machine();
        let _admitting = lending.admitting.lock().await;
        let granted = lock(&lending.granted).clone();
        match self.access.verdict(&dir, &granted) {
            Verdict::Allowed => return Ok(()),
            Verdict::Excluded(excluded) => {
                return Err(format!(
                    "{} is in {}, which the user's configuration keeps the host's agents out of \
                     on `{machine}`",
                    dir.display(),
                    excluded.display()
                ));
            }
            Verdict::Ask => {}
        }
        let ask = Ask::Directory {
            machine: machine.to_owned(),
            path: dir.display().to_string(),
        };
        match reporter.ask(ask).await {
            Decision::Once => {}
            Decision::Always => {
                if let Err(error) = self.access.allow_always(&dir) {
                    let text = format!("{} allowed for this conversation only: {error:#}", dir.display());
                    reporter.emit(Event::Notice { text }).await;
                }
            }
            Decision::Decline => {
                return Err(format!(
                    "The user did not let the agent use {} on `{machine}`.",
                    dir.display()
                ));
            }
            Decision::Stop => {
                reporter.stop().await;
                return Err("The user stopped the run.".into());
            }
        }
        lock(&lending.granted).push(dir);
        Ok(())
    }

    fn lending(&self, thread: &SessionId) -> Arc<Lending> {
        let mut conversations = lock(&self.conversations);
        let lending = conversations.entry(thread.clone()).or_insert_with(|| {
            Arc::new(Lending {
                permissions: self.permissions.fork(),
                granted: Mutex::default(),
                admitting: tokio::sync::Mutex::default(),
                seen: Seen::default(),
            })
        });
        lending.clone()
    }
}

/// Runs a call the host gave this machine, unless another terminal here
/// takes it up first. What it shows and asks goes through the host's run,
/// as for a call on the host.
pub async fn take_up(
    client: Client,
    workbench: Arc<Workbench>,
    thread: SessionId,
    follower: FollowerId,
    id: CallId,
    call: Call,
) {
    if !client.claim(&thread, id, follower).await.unwrap_or(false) {
        return;
    }
    let relay = Relay {
        client: client.clone(),
        thread: thread.clone(),
        id,
    };
    let result = workbench.run(&thread, &call, &relay).await;
    // If the host cannot hear it, the call fails once this terminal stops
    // following the run.
    _ = client.finish(&thread, id, &result).await;
}

/// Reports a call's output and questions to the host's run.
struct Relay {
    client: Client,
    thread: SessionId,
    id: CallId,
}

impl Reporter for Relay {
    async fn emit(&self, event: Event) {
        _ = self.client.call_event(&self.thread, self.id, &event).await;
    }

    /// A question the host cannot take, as when its run ended, stops the call.
    async fn ask(&self, ask: Ask) -> Decision {
        self.client
            .call_ask(&self.thread, self.id, &ask)
            .await
            .unwrap_or(Decision::Stop)
    }

    async fn stop(&self) {
        _ = self.client.stop(&self.thread).await;
    }
}

/// Shows each piece of output `live` brings, in order, until it ends.
async fn forward(mut live: mpsc::UnboundedReceiver<String>, reporter: &impl Reporter) {
    while let Some(text) = live.recv().await {
        reporter.emit(Event::Output { text }).await;
    }
}

/// The directories a call to the built-in `tool` with `args` works in, as
/// the user is asked about them; none for any other tool, whose reach is
/// not known.
fn directories(tool: &str, args: &Value, cwd: &Path) -> Vec<PathBuf> {
    let field = |key| args.get(key).and_then(Value::as_str);
    let paths = match tool {
        Read::NAME | Write::NAME | Edit::NAME | Delete::NAME => vec![field("path")],
        List::NAME | Search::NAME => vec![field("path").or(Some("."))],
        Move::NAME => vec![field("from"), field("to")],
        Shell::NAME => vec![field("cwd").or(Some("."))],
        _ => Vec::new(),
    };
    let mut dirs = Vec::new();
    for dir in paths.into_iter().flatten().map(|path| directory(path, cwd)) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The directory a call on `path` works in: the working directory when the
/// path is inside it, else the path itself when it is a directory, else
/// the directory it is in.
fn directory(path: &str, cwd: &Path) -> PathBuf {
    let path = Local.canonical(&cwd.join(shellexpand::tilde(path).as_ref()));
    if path.starts_with(cwd) {
        return cwd.to_path_buf();
    }
    match path.is_dir() {
        true => path,
        false => path.parent().map_or_else(|| path.clone(), Path::to_path_buf),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn calls_ask_for_the_working_directory_or_the_directory_outside_it_they_touch() {
        let root = std::env::temp_dir().join(format!("hey-workbench-{}", std::process::id()));
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        let root = root.canonicalize().unwrap();
        let cwd = root.join("project");
        let dirs = |tool: &str, args: Value| directories(tool, &args, &cwd);

        let here = std::slice::from_ref(&cwd);
        assert_eq!(dirs("read", json!({ "path": "src/main.rs" })), here);
        assert_eq!(dirs("list", json!({})), here, "no path lists the working directory");
        assert_eq!(dirs("shell", json!({ "command": "ls /" })), here);
        assert_eq!(dirs("write", json!({ "path": "../elsewhere/new/file.txt" })), [root.join("elsewhere/new")]);
        assert_eq!(dirs("list", json!({ "path": "../elsewhere" })), [root.join("elsewhere")]);
        assert_eq!(
            dirs("move", json!({ "from": "src/a.rs", "to": "../elsewhere/a.rs" })),
            [cwd.clone(), root.join("elsewhere")]
        );
        assert!(dirs("fetch", json!({ "url": "https://example.com" })).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
