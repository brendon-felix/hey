/* -------------------------------------------------------------------------- */
/*                                processes.rs                                */
/* -------------------------------------------------------------------------- */

//! Commands the agent leaves running in the background, such as a server or
//! a watcher: `spawn_process` starts one in a process group of its own,
//! `process_output` reads what it printed, `process_list` lists them and
//! `process_kill` stops one. Each machine keeps its own, in this process,
//! and kills them all when hey exits.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use rig_tools::process::{Launcher, Process, STOP_GRACE, Spawn};
use rig_tools::workspace::Workspace;
use rig_tools::{OutputLimit, Spill};
use schemars::JsonSchema;
use serde::Deserialize;

/// Most bytes of a process's latest output kept in memory; all of it is
/// kept in its spill file.
const KEPT_BYTES: usize = 256 * 1024;

/// Longest `process_output` waits.
const MAX_WAIT: Duration = Duration::from_secs(120);

/// How often a process is checked for output and its end.
const POLL: Duration = Duration::from_millis(50);

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static TABLE: Mutex<BTreeMap<u32, Arc<Background>>> = Mutex::new(BTreeMap::new());

/// A command running, or that ran, in the background.
struct Background {
    command: String,
    dir: Option<PathBuf>,
    started: Instant,
    process: Mutex<Process>,
    state: Mutex<State>,
}

/// What a background process printed, and how it ended.
#[derive(Default)]
struct State {
    /// Its latest output, at most [`KEPT_BYTES`].
    kept: VecDeque<u8>,
    /// Bytes printed in all; `kept` ends here.
    total: u64,
    /// Where all of it is kept, when a file could be made.
    spill: Option<(PathBuf, File)>,
    ended: Option<String>,
}

impl State {
    fn push(&mut self, text: &str) {
        if let Some((_, file)) = &mut self.spill {
            _ = file.write_all(text.as_bytes());
        }
        self.kept.extend(text.as_bytes());
        let over = self.kept.len().saturating_sub(KEPT_BYTES);
        self.kept.drain(..over);
        self.total += text.len() as u64;
    }

    /// The offset `kept` starts at.
    fn first(&self) -> u64 {
        self.total - self.kept.len() as u64
    }

    /// What was printed from `offset` on, of what is kept.
    fn since(&self, offset: u64) -> String {
        let skip = offset
            .saturating_sub(self.first())
            .min(self.kept.len() as u64) as usize;
        let bytes: Vec<u8> = self.kept.iter().skip(skip).copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Background {
    /// Takes what the process printed since last time, and notes its end.
    fn pump(&self) {
        let mut process = lock(&self.process);
        let mut state = lock(&self.state);
        if state.ended.is_some() {
            return;
        }
        while let Ok(line) = process.lines().try_recv() {
            state.push(line.text());
        }
        match process.try_wait() {
            Ok(Some(status)) => {
                while let Ok(line) = process.lines().try_recv() {
                    state.push(line.text());
                }
                state.ended = Some(ended(status));
            }
            Ok(None) => {}
            Err(error) => state.ended = Some(format!("lost: {error}")),
        }
    }

    fn kill(&self) -> String {
        self.pump();
        if let Some(ended) = &lock(&self.state).ended {
            return format!("had already ended ({ended})");
        }
        let status = lock(&self.process).stop(STOP_GRACE);
        let mut state = lock(&self.state);
        let how = match status {
            Ok(status) => format!("killed ({})", ended(status)),
            Err(error) => format!("killed ({error})"),
        };
        state.ended = Some(how.clone());
        how
    }

    fn status(&self) -> String {
        match &lock(&self.state).ended {
            Some(ended) => ended.clone(),
            None => format!("running for {}s", self.started.elapsed().as_secs()),
        }
    }
}

fn ended(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {code}"),
        None => format!("ended: {status}"),
    }
}

/// Starts `command` with `sh -c` in `dir` of `workspace`, in the background.
fn spawn(
    workspace: &dyn Workspace,
    launcher: &Launcher,
    dir: Option<&Path>,
    command: &str,
    spill: Option<&Spill>,
) -> std::io::Result<u32> {
    let how = Spawn {
        stdin: None,
        merge_stderr: true,
    };
    let args = [OsString::from("-c"), OsString::from(command)];
    let process = workspace.spawn(launcher, dir, OsStr::new("sh"), &args, how)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let state = State {
        spill: spill.and_then(|spill| spill.create(&format!("process-{id}")).ok()),
        ..State::default()
    };
    let background = Arc::new(Background {
        command: command.to_owned(),
        dir: dir.or(launcher.cwd.as_deref()).map(Path::to_path_buf),
        started: Instant::now(),
        process: Mutex::new(process),
        state: Mutex::new(state),
    });
    // Drains its output as it comes, so a chatty process never blocks on a
    // full pipe, until it ends.
    let pumped = Arc::downgrade(&background);
    std::thread::Builder::new()
        .name("hey-background".into())
        .spawn(move || {
            while let Some(background) = pumped.upgrade() {
                background.pump();
                if lock(&background.state).ended.is_some() {
                    break;
                }
                drop(background);
                std::thread::sleep(POLL);
            }
        })?;
    lock(&TABLE).insert(id, background);
    Ok(id)
}

fn find(id: u32) -> Result<Arc<Background>, ToolExecutionError> {
    lock(&TABLE).get(&id).cloned().ok_or_else(|| {
        ToolExecutionError::invalid_args(format!(
            "no background process {id}; `process_list` lists them"
        ))
    })
}

/// Stops every background process this hey started, as it exits.
pub fn kill_all() {
    let all: Vec<_> = std::mem::take(&mut *lock(&TABLE)).into_values().collect();
    for background in all {
        if lock(&background.state).ended.is_none() {
            let _ = lock(&background.process).kill();
        }
    }
}

/// Starts a command in the background.
pub struct SpawnProcess {
    pub workspace: Arc<dyn Workspace>,
    pub launcher: Launcher,
    pub confine: bool,
    pub spill: Option<Spill>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnArgs {
    /// The command, run with `sh -c`.
    pub command: String,
    /// The directory it runs in; default the current directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Tool for SpawnProcess {
    const NAME: &'static str = "spawn_process";
    type Args = SpawnArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Start a command with `sh -c` in the background, in a process group of its own, and \
         return its id at once. Use it for what runs on, such as a server, a watcher or a long \
         build, not `shell`; read its output with `process_output` and stop it with \
         `process_kill`. It has no stdin, and is killed when hey exits."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<SpawnArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_title("Spawn process")
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: SpawnArgs,
    ) -> Result<String, ToolExecutionError> {
        let base = self.launcher.cwd.as_deref();
        let dir = match &args.cwd {
            Some(dir) => {
                if self.confine {
                    self.workspace.place(base, true, dir)?;
                }
                Some(
                    self.workspace
                        .join(base.unwrap_or(Path::new("")), Path::new(dir)),
                )
            }
            None => base.map(Path::to_path_buf),
        };
        let id = spawn(
            self.workspace.as_ref(),
            &self.launcher,
            dir.as_deref(),
            &args.command,
            self.spill.as_ref(),
        )
        .map_err(|error| ToolExecutionError::other(format!("could not run sh: {error}")))?;
        Ok(format!(
            "Started background process {id}. Read its output with `process_output`, and stop it with `process_kill`."
        ))
    }
}

/// Reads what a background process printed.
pub struct ProcessOutput;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputArgs {
    /// The process's id, as `spawn_process` gave it.
    pub id: u32,
    /// Only output from this byte offset on, as an earlier call's `next offset` gave it.
    #[serde(default)]
    pub since: Option<u64>,
    /// Only the last this many lines.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Wait up to this many seconds (at most 120) for new output or the process's end.
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

impl Tool for ProcessOutput {
    const NAME: &'static str = "process_output";
    type Args = OutputArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Read what a background process started with `spawn_process` printed, stdout and stderr \
         together, and whether it still runs. Pass `since` as the `next offset` an earlier call \
         gave for only what is new, `tail` for the last lines, and `wait_secs` to wait for new \
         output or its end. All of its output is also kept in a file it names."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<OutputArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: OutputArgs,
    ) -> Result<String, ToolExecutionError> {
        let background = find(args.id)?;
        let since = args.since.unwrap_or(0);
        let wait = Duration::from_secs(args.wait_secs.unwrap_or(0)).min(MAX_WAIT);
        let deadline = Instant::now() + wait;
        loop {
            background.pump();
            let done = {
                let state = lock(&background.state);
                state.ended.is_some() || state.total > since
            };
            if done || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        Ok(report(args.id, &background, since, args.tail))
    }
}

/// What `process_output` says of `background` from `since` on.
fn report(id: u32, background: &Background, since: u64, tail: Option<usize>) -> String {
    let status = background.status();
    let state = lock(&background.state);
    let mut notes = vec![format!("process {id}: {status}")];
    if since < state.first() {
        notes.push(format!(
            "output before offset {} is no longer held{}",
            state.first(),
            spilled(&state)
        ));
    }
    let text = state.since(since);
    let limit = OutputLimit {
        lines: tail.unwrap_or(rig_tools::MAX_LINES),
        ..OutputLimit::default()
    };
    let (shown, dropped) = limit.tail(&text);
    if dropped > 0 {
        notes.push(format!(
            "{dropped} earlier lines left out{}",
            spilled(&state)
        ));
    }
    notes.push(format!("next offset: {}", state.total));
    let body = match shown.is_empty() {
        true => "(no new output)\n".to_owned(),
        false if shown.ends_with('\n') => shown.to_owned(),
        false => format!("{shown}\n"),
    };
    format!("{body}[{}]", notes.join("; "))
}

fn spilled(state: &State) -> String {
    match &state.spill {
        Some((path, _)) => format!("; all of it is in {}", path.display()),
        None => String::new(),
    }
}

/// Lists the background processes.
pub struct ProcessList;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {}

impl Tool for ProcessList {
    const NAME: &'static str = "process_list";
    type Args = ListArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "List the background processes `spawn_process` started: id, status, directory and command."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<ListArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
            .with_read_only(true)
            .with_open_world(false)
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: ListArgs,
    ) -> Result<String, ToolExecutionError> {
        let all: Vec<_> = lock(&TABLE)
            .iter()
            .map(|(id, background)| (*id, background.clone()))
            .collect();
        if all.is_empty() {
            return Ok("No background processes.".into());
        }
        Ok(all
            .iter()
            .map(|(id, background)| {
                background.pump();
                let dir = background
                    .dir
                    .as_ref()
                    .map(|dir| format!(" in {}", dir.display()))
                    .unwrap_or_default();
                format!("{id}: {}{dir}: {}", background.status(), background.command)
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Stops a background process.
pub struct ProcessKill;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KillArgs {
    /// The process's id, as `spawn_process` gave it.
    pub id: u32,
}

impl Tool for ProcessKill {
    const NAME: &'static str = "process_kill";
    type Args = KillArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        "Stop a background process started with `spawn_process`, and all it started: asked to \
         stop, then killed two seconds later if it still runs."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<KillArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_title("Kill process")
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: KillArgs,
    ) -> Result<String, ToolExecutionError> {
        let background = find(args.id)?;
        let how = rig_tools::blocking(move || Ok(background.kill())).await?;
        Ok(format!("process {}: {how}", args.id))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use rig_tools::workspace::Local;

    use super::*;

    #[test]
    fn the_ring_keeps_the_latest_output_and_counts_all_of_it() {
        let mut state = State::default();
        state.push(&"a".repeat(KEPT_BYTES));
        state.push("tail\n");
        assert_eq!(state.total, KEPT_BYTES as u64 + 5);
        assert_eq!(state.first(), 5);
        assert!(state.since(0).ends_with("tail\n"));
        assert_eq!(state.since(KEPT_BYTES as u64), "tail\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_process_is_read_waited_for_and_killed() {
        let mut context = ToolContext::default();
        let id = spawn(
            &Local,
            &Launcher::default(),
            None,
            "echo one; sleep 30",
            None,
        )
        .unwrap();
        let args = OutputArgs {
            id,
            since: None,
            tail: None,
            wait_secs: Some(5),
        };
        let read = ProcessOutput.call(&mut context, args).await.unwrap();
        assert!(read.starts_with("one\n"), "{read}");
        assert!(read.contains("running"), "{read}");
        assert!(read.contains("next offset: 4"), "{read}");

        let listed = ProcessList.call(&mut context, ListArgs {}).await.unwrap();
        assert!(listed.contains("sleep 30"), "{listed}");

        let killed = ProcessKill
            .call(&mut context, KillArgs { id })
            .await
            .unwrap();
        assert!(killed.contains("killed"), "{killed}");
        let args = OutputArgs {
            id,
            since: Some(4),
            tail: None,
            wait_secs: None,
        };
        let read = ProcessOutput.call(&mut context, args).await.unwrap();
        assert!(read.starts_with("(no new output)"), "{read}");
    }
}
