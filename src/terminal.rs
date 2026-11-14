/* -------------------------------------------------------------------------- */
/*                                 terminal.rs                                */
/* -------------------------------------------------------------------------- */

//! The thread a terminal's messages from the command line go on, so each
//! `hey MESSAGE` follows on from the last one sent in the same terminal.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::sessions::SessionId;
use crate::{config, render};

const FILE: &str = "terminals.json";

/// The thread this terminal's messages go on, which its first one starts;
/// a new one for every message when the terminal cannot be told.
pub fn thread() -> SessionId {
    let Ok(mut system) = System::new() else {
        return SessionId::new();
    };
    let Some(terminal) = Terminal::current(&mut system) else {
        return SessionId::new();
    };
    kept(&mut system, terminal).unwrap_or_else(|error| {
        render::notice(&format!("in a new thread, not this terminal's: {error:#}"));
        SessionId::new()
    })
}

fn kept(system: &mut System, terminal: Terminal) -> Result<SessionId> {
    let file = config::data_dir()?.join(FILE);
    let mut terminals = Terminals::read(&file)?;
    terminals.0.retain(|kept| kept.terminal.is_open(system));
    let thread = terminals.thread(terminal);
    terminals
        .write(&file)
        .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(thread)
}

/// A terminal, as the process it was started with: on Unix the leader of
/// hey's session, the shell the terminal started, or what started it;
/// elsewhere hey's parent, the shell hey was typed in. When it started
/// tells it apart from a later process given the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Terminal {
    pid: u32,
    started: u64,
}

impl Terminal {
    fn current(system: &mut System) -> Option<Self> {
        let hey = process(system, sysinfo::get_current_pid().ok()?)?;
        #[cfg(unix)]
        let pid = hey.session_id()?;
        #[cfg(not(unix))]
        let pid = hey.parent()?;
        Self::of(system, pid)
    }

    fn of(system: &mut System, pid: Pid) -> Option<Self> {
        let started = process(system, pid)?.start_time();
        Some(Self {
            pid: pid.as_u32(),
            started,
        })
    }

    fn is_open(self, system: &mut System) -> bool {
        Self::of(system, Pid::from_u32(self.pid)) == Some(self)
    }
}

/// The process `pid` as it is now, if it runs.
fn process(system: &mut System, pid: Pid) -> Option<&sysinfo::Process> {
    let only = ProcessesToUpdate::Some(&[pid]);
    system.refresh_processes_specifics(only, true, ProcessRefreshKind::nothing());
    system.process(pid)
}

/// Each open terminal's thread.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
struct Terminals(Vec<Kept>);

#[derive(Serialize, Deserialize)]
struct Kept {
    terminal: Terminal,
    thread: SessionId,
}

impl Terminals {
    fn read(file: &Path) -> Result<Self> {
        match fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("cannot read {}", file.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", file.display())),
        }
    }

    /// Replaces the file whole, through a file of this process's own, so
    /// terminals sending at once cannot interleave what they write.
    fn write(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let written = file.with_extension(format!("json.{}", std::process::id()));
        fs::write(&written, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&written, file)?;
        Ok(())
    }

    /// The thread of `terminal`, started now if it has none.
    fn thread(&mut self, terminal: Terminal) -> SessionId {
        match self.0.iter().find(|kept| kept.terminal == terminal) {
            Some(kept) => kept.thread.clone(),
            None => {
                let thread = SessionId::new();
                self.0.push(Kept {
                    terminal,
                    thread: thread.clone(),
                });
                thread
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_is_open_only_as_the_process_it_started_with() {
        let mut system = System::new().unwrap();
        let terminal = Terminal::current(&mut system).expect("tests run in a session");
        assert!(terminal.is_open(&mut system));
        let reused = Terminal {
            started: terminal.started - 1,
            ..terminal
        };
        assert!(!reused.is_open(&mut system), "another process with its id");
    }
}
