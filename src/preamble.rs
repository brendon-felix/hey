/* -------------------------------------------------------------------------- */
/*                                preamble.rs                                 */
/* -------------------------------------------------------------------------- */

//! What hey tells the model after the system prompt: what it is, which
//! machine its tools act on, and where the user is, so it neither guesses
//! at its surroundings nor reaches for files on the wrong machine.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::protocol::{ClientInfo, Machine};
use crate::sessions::SessionId;
use crate::skills::Source;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");
const DEFAULTS: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/defaults.toml");

/// Where a host runs the agent.
#[derive(Debug, Clone, Copy)]
pub enum Hosting {
    /// In the user's own `hey`, on their machine.
    Local,
    /// Behind `hey --serve`, for clients on other machines.
    Served { listen: SocketAddr },
}

/// Everything the model is told about one run's surroundings.
pub struct Setting<'a> {
    pub hosting: Hosting,
    pub host: &'a Machine,
    pub client: Option<&'a ClientInfo>,
    pub model: &'a str,
    /// Kept for the whole conversation rather than the run's own time, so
    /// the preamble stays the same and the provider's prompt cache works.
    pub started: Option<NaiveDateTime>,
    pub thread: &'a SessionId,
    /// Where conversations are saved.
    pub sessions: &'a Path,
    pub shared: &'a Path,
    /// Where the host keeps copies the user's clients synced.
    pub synced: &'a Path,
    /// The memories file; `None` when memory is off.
    pub memories: Option<&'a Path>,
    /// Where skills are looked for, farthest first.
    pub skills: &'a [(Source, PathBuf)],
    /// Where tools keep whole the output they cut.
    pub spill: Option<&'a Path>,
}

impl Setting<'_> {
    pub fn render(&self) -> String {
        let lines = [
            Some(format!(
                "You are hey {VERSION}, a command-line agent ({HOMEPAGE}), answering as `{}`.",
                self.model
            )),
            Some(self.host_line()),
            self.user_line(),
            self.system_line(),
            self.windows_line(),
            self.started.map(|started| {
                format!(
                    "This conversation started {} in the user's local time, and may have gone \
                     on since; run `date` for the current time.",
                    started.format("%A %Y-%m-%d %H:%M")
                )
            }),
            self.host
                .config
                .as_deref()
                .map(|config| self.config_line(config)),
            self.client
                .and_then(|client| client.machine.config.as_deref())
                .map(|config| {
                    format!(
                        "The user's own config file, on their machine, sets their display, \
                         editor and remote: {config}."
                    )
                }),
            Some(format!(
                "Conversations are saved in {}, a log each, named by its id; this one is `{}`.",
                self.sessions.display(),
                self.thread.as_str()
            )),
            Some(format!(
                "`@shared:` mentions attach from {}, a repository on the host every client can \
                 use.",
                self.shared.display()
            )),
            Some(format!(
                "Files, folders and git repositories the user syncs from their machine are \
                 copied to {}/<machine>/<name>, while their client is connected. Work there \
                 when their machine is out of reach; the copies are one way, so tell the user \
                 what you changed, and expect them to be a little behind their machine.",
                self.synced.display()
            )),
            Some(self.memories_line()),
            self.skills_line(),
            Some(
                "Tool arguments are one JSON object each. `edit` takes `path` and either `old_text` \
                 and `new_text` for one change (prefer this), or `edits`, a real JSON array of \
                 such pairs, not a string of JSON; keep each edit small, and use `write` to \
                 replace a whole file."
                    .into(),
            ),
            Some(
                "Your replies are printed to the terminal as raw Markdown, so Markdown tables \
                 come out misaligned. To show a table, call the `table` tool and paste its \
                 output verbatim in a code block; never write a Markdown table yourself."
                    .into(),
            ),
            self.spill.map(|dir| {
                format!(
                    "Tool output too long to give you whole is kept whole in {}.",
                    dir.display()
                )
            }),
        ];
        let bullets: String = lines
            .into_iter()
            .flatten()
            .map(|line| format!("\n- {line}"))
            .collect();
        format!("\nSetting:{bullets}")
    }

    fn memories_line(&self) -> String {
        match self.memories {
            Some(file) => format!(
                "Memories of the user are kept in {}, a Markdown list you may edit when asked, \
                 with their embeddings in {}.",
                file.display(),
                file.with_extension("vectors.json").display()
            ),
            None => "Memory is off (`memory.enabled` in the config), so nothing is remembered \
                      across conversations."
                .into(),
        }
    }

    fn skills_line(&self) -> Option<String> {
        let dirs = |source: Source| {
            let dirs: Vec<String> = self
                .skills
                .iter()
                .filter(|(of, _)| *of == source)
                .map(|(_, dir)| dir.display().to_string())
                .collect();
            (!dirs.is_empty()).then(|| dirs.join(", "))
        };
        let found = match (dirs(Source::Global), dirs(Source::Project)) {
            (None, None) => return None,
            (Some(global), None) => format!("in {global}"),
            (None, Some(project)) => format!("for this project in {project}"),
            (Some(global), Some(project)) => {
                format!("in {global} for every project, and for this one in {project}")
            }
        };
        Some(format!(
            "Agent Skills (https://agentskills.io) are found {found}, each a folder named after \
             it holding a SKILL.md; a nearer one replaces a farther one of the same name. The \
             user invokes one with `/name` or `@skill:name`, and `/skills` lists them."
        ))
    }

    fn host_line(&self) -> String {
        let host = describe(self.host);
        let cwd = self.host.cwd.as_deref().unwrap_or("an unknown directory");
        match self.hosting {
            Hosting::Local => format!(
                "You run in the user's terminal on their machine, {host}. Your tools act on it, \
                 from {cwd}."
            ),
            Hosting::Served { listen } => format!(
                "You run on a hey host, {host}, served by `hey --serve` listening on {listen}. \
                 Your tools act on the host, from {cwd}, or on the machine a call names."
            ),
        }
    }

    fn user_line(&self) -> Option<String> {
        let unreachable = "You cannot reach files on their machine; those they mention with `@` \
                           arrive attached to their message.";
        match (self.hosting, self.client) {
            (
                _,
                Some(ClientInfo {
                    machine,
                    remote,
                    url,
                    lent,
                }),
            ) => {
                let cwd = machine
                    .cwd
                    .as_deref()
                    .map(|cwd| format!(", working in {cwd},"))
                    .unwrap_or_default();
                let reach = match lent {
                    Some(lent) => format!(
                        "Tools with a `machine` argument run on the machine each call names: the \
                         host, which is where a call that leaves it out runs, or theirs, `{}`, \
                         while they have this conversation open there. Name theirs for paths \
                         on their machine. \
                         Tools described as running there run only there; files they mention \
                         with `@` arrive attached to their message.",
                        lent.machine
                    ),
                    None => unreachable.to_owned(),
                };
                let reach = format!(
                    "{reach} Their client may drop out at any time (closed laptop, lost \
                     connection) and return later; when a call to their machine fails because \
                     it is gone, do not stop or wait on them: carry on with what the host can \
                     do, use what is already synced to the host, and say what remains for when \
                     they reconnect."
                );
                Some(format!(
                    "The user is on {}{cwd} and reaches the host as remote `{remote}` at {url}. \
                     {reach}",
                    describe(machine)
                ))
            }
            (Hosting::Served { .. }, None) => Some(format!(
                "The user reaches the host from another machine, which their client did not \
                 describe. {unreachable}"
            )),
            (Hosting::Local, None) => None,
        }
    }

    /// The operating system of the client that started the conversation, which
    /// may differ from the host's.
    fn system_line(&self) -> Option<String> {
        let client = self.client?;
        Some(format!(
            "The client that started this conversation runs {} ({}); {}",
            os_name(&client.machine.os),
            client.machine.arch,
            match client.machine.os.as_str() {
                "windows" => "expect Windows paths and conventions on their machine.",
                _ => "expect that system's paths and conventions on their machine.",
            }
        ))
    }

    /// Shell guidance, for whichever machines in play run Windows.
    fn windows_line(&self) -> Option<String> {
        let host_windows = self.host.os == "windows";
        let client_windows = self.client.is_some_and(|c| c.machine.os == "windows");
        let on = match (host_windows, client_windows) {
            (false, false) => return None,
            (true, true) => "the host and the user's machine run",
            (true, false) => "the host runs",
            (false, true) => "the user's machine runs",
        };
        Some(format!(
            "Because {on} Windows, a `shell` call there must use a shell and commands Windows \
             supports: do not assume `sh`, bash, or Unix tools such as `ls`, `cat`, `grep`, \
             `sed`, `head` or `rm`, and do not write `&&`, `$(...)`, `/dev/null` or \
             `~` paths. Use PowerShell or cmd syntax, or the shell the tool's description \
             names (for example Nushell), and Windows paths. Prefer the dedicated `read`, \
             `write`, `edit`, `list`, `search`, `move` and `delete` tools over shell \
             commands. If a call fails with `program not found`, the shell is not available \
             there; switch to a supported one instead of retrying."
        ))
    }

    fn config_line(&self, config: &str) -> String {
        let missing = match Path::new(config).exists() {
            true => "",
            false => " (not created yet, so every option has its default)",
        };
        let (whose, restart) = match self.hosting {
            Hosting::Local => ("hey's", "the next time hey starts"),
            Hosting::Served { .. } => ("The host's", "once `hey --serve` restarts"),
        };
        format!(
            "{whose} config file is {config}{missing}. It sets the model, tools and their \
             permissions, MCP servers and memory; every option, with its default, is documented \
             at {DEFAULTS}. Changes take effect {restart}."
        )
    }
}

fn os_name(os: &str) -> &str {
    match os {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// `name` (os, arch), or the platform alone when the name is unknown.
fn describe(machine: &Machine) -> String {
    let platform = format!("{}, {}", machine.os, machine.arch);
    match &machine.hostname {
        Some(name) => format!("`{name}` ({platform})"),
        None => format!("a {platform} machine"),
    }
}
