/* -------------------------------------------------------------------------- */
/*                                   wake.rs                                  */
/* -------------------------------------------------------------------------- */

//! Continuing a conversation by itself once the user's machine is back. The
//! agent still gets as much done as it can while the client is away; when
//! what remains needs that machine, it leaves a [`Wake`] with the
//! `resume_on_reconnect` tool and ends its reply. The host keeps the wake
//! beside the conversation's notes, so it outlasts a restart, and starts a
//! new run from it when a client on that machine connects.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host::Run;
use crate::protocol::{ClientInfo, Event, RunOptions, Surface};
use crate::sessions::SessionId;

pub const TOOL: &str = "resume_on_reconnect";

const GUIDANCE: &str = "Has the host continue this conversation by itself, with a new run, when \
the user's machine reconnects. Use it when work is left that needs their machine (its tools, \
files or approval) and it is not connected now; then finish what you can without it and end your \
reply. `task` says what to do once it is back, as a note to yourself: you will not remember this \
run, only the conversation and this note. Replaces an earlier note for this conversation.";

/// What the host does once `machine` is back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wake {
    pub machine: String,
    /// The agent's note to itself.
    pub task: String,
    /// How the run that left it was set up, which the next is too.
    pub options: RunOptions,
    pub client: ClientInfo,
    pub surface: Surface,
}

impl Wake {
    /// A wake for a run the host's restart cut off, which has no machine to
    /// wait for: the host starts it again as soon as it is back up.
    pub fn restart(options: RunOptions, client: ClientInfo, surface: Surface) -> Self {
        Self {
            machine: String::new(),
            task: String::new(),
            options,
            client,
            surface,
        }
    }

    /// What the new run is asked.
    pub fn prompt(&self) -> String {
        if self.machine.is_empty() {
            return "The hey host restarted while you were answering, which stopped your reply. \
                    Carry on from where you left off, unless it was already done."
                .into();
        }
        format!(
            "`{}` reconnected. You asked to be continued when it did: {}",
            self.machine, self.task
        )
    }
}

/// The wakes waiting, and the machines with a client connected.
pub struct Wakes {
    dir: Option<PathBuf>,
    waiting: Mutex<HashMap<SessionId, Wake>>,
    present: Mutex<HashMap<String, usize>>,
}

impl Wakes {
    /// The wakes saved in `dir`; without one, kept only in memory.
    pub fn open(dir: Option<PathBuf>) -> Self {
        let mut waiting = HashMap::new();
        for entry in dir
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<SessionId>().ok());
            let wake = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Wake>(&text).ok());
            if let (Some(id), Some(wake)) = (id, wake) {
                waiting.insert(id, wake);
            }
        }
        Self {
            dir,
            waiting: Mutex::new(waiting),
            present: Mutex::default(),
        }
    }

    fn file(&self, id: &SessionId) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{}.json", id.as_str())))
    }

    /// Leaves `wake` for thread `id`, in place of any it had.
    pub fn set(&self, id: &SessionId, wake: Wake) {
        if let Some(file) = self.file(id) {
            match serde_json::to_vec(&wake) {
                Ok(json) => {
                    if let Err(error) = crate::fsio::write_atomic(&file, json) {
                        tracing::warn!(file = %file.display(), %error, "cannot save a wake");
                    }
                }
                Err(error) => tracing::warn!(%error, "cannot serialize a wake"),
            }
        }
        self.waiting().insert(id.clone(), wake);
    }

    /// Takes thread `id`'s wake, if it has one.
    pub fn take(&self, id: &SessionId) -> Option<Wake> {
        let wake = self.waiting().remove(id)?;
        if let Some(file) = self.file(id) {
            _ = std::fs::remove_file(file);
        }
        Some(wake)
    }

    pub fn forget(&self, id: &SessionId) {
        self.take(id);
    }

    #[cfg(test)]
    pub fn pending(&self, id: &SessionId) -> bool {
        self.waiting().contains_key(id)
    }

    /// The threads waiting for `machine`.
    pub fn waiting_for(&self, machine: &str) -> Vec<SessionId> {
        self.waiting()
            .iter()
            .filter(|(_, wake)| wake.machine == machine)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// A client on `machine` connected, until the returned guard is dropped.
    pub fn arrive(self: &Arc<Self>, machine: String) -> Presence {
        *self.present().entry(machine.clone()).or_default() += 1;
        Presence {
            wakes: self.clone(),
            machine,
        }
    }

    /// Whether a client on `machine` is connected.
    pub fn is_present(&self, machine: &str) -> bool {
        self.present().get(machine).is_some_and(|count| *count > 0)
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Wake>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn present(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.present.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A client's connection, which counts its machine as present until dropped.
pub struct Presence {
    wakes: Arc<Wakes>,
    machine: String,
}

impl Drop for Presence {
    fn drop(&mut self) {
        let mut present = self.wakes.present();
        if let Some(count) = present.get_mut(&self.machine) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                present.remove(&self.machine);
            }
        }
    }
}

/// The tool that leaves a wake for the conversation.
pub struct Resume {
    pub wakes: Arc<Wakes>,
    pub thread: SessionId,
    pub run: Arc<Run>,
    pub client: ClientInfo,
    pub machine: String,
    pub options: RunOptions,
    pub surface: Surface,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// What to do once the machine is back, as a note to yourself.
    pub task: String,
}

impl Tool for Resume {
    const NAME: &'static str = TOOL;
    type Args = Args;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<Args>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_open_world(false)
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Args,
    ) -> Result<String, ToolExecutionError> {
        let machine = &self.machine;
        if self.wakes.is_present(machine) {
            return Err(ToolExecutionError::new(
                ToolErrorKind::Other,
                format!(
                    "`{machine}` is connected now, so there is nothing to wait for: use its tools \
                     directly, or `ask_user` if the user has to open this conversation there"
                ),
            ));
        }
        self.wakes.set(
            &self.thread,
            Wake {
                machine: machine.clone(),
                task: args.task,
                options: self.options.clone(),
                client: self.client.clone(),
                surface: self.surface,
            },
        );
        self.run.emit(Event::Notice {
            text: format!("this conversation continues by itself when `{machine}` reconnects"),
        });
        Ok(format!(
            "Saved. When `{machine}` reconnects, the host starts a new run in this conversation \
             with your note. Do what you can without it now, then end your reply."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wake(machine: &str) -> Wake {
        Wake {
            machine: machine.into(),
            task: "run the tests".into(),
            options: RunOptions {
                model: "test/model".into(),
                thinking: None,
                system_prompt: None,
            },
            client: ClientInfo {
                machine: crate::protocol::Machine::here(None),
                remote: "fermi".into(),
                url: "http://host/".into(),
                lent: None,
                skills: Vec::new(),
                project: None,
            },
            surface: Surface::Repl,
        }
    }

    #[test]
    fn a_wake_outlasts_the_host_and_is_taken_once() {
        let dir = std::env::temp_dir().join(format!("hey-wake-test-{}", std::process::id()));
        _ = std::fs::create_dir_all(&dir);
        let id = SessionId::new();
        let wakes = Wakes::open(Some(dir.clone()));
        wakes.set(&id, wake("turing"));
        assert_eq!(wakes.waiting_for("turing"), vec![id.clone()]);
        assert!(wakes.waiting_for("other").is_empty());

        let again = Wakes::open(Some(dir.clone()));
        assert!(again.pending(&id));
        assert_eq!(again.take(&id).unwrap().task, "run the tests");
        assert!(again.take(&id).is_none());
        assert!(!Wakes::open(Some(dir.clone())).pending(&id));
        _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_machine_is_present_while_any_client_on_it_is_connected() {
        let wakes = Arc::new(Wakes::open(None));
        assert!(!wakes.is_present("turing"));
        let first = wakes.arrive("turing".into());
        let second = wakes.arrive("turing".into());
        drop(first);
        assert!(wakes.is_present("turing"));
        drop(second);
        assert!(!wakes.is_present("turing"));
    }
}
