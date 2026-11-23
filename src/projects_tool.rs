/* -------------------------------------------------------------------------- */
/*                              projects_tool.rs                              */
/* -------------------------------------------------------------------------- */

//! The `project` tool: where a project lives, whether its copies match,
//! and, with the user's consent, a copy on the host.

use std::path::Path;
use std::sync::Arc;

use rig_core::completion::ToolAnnotations;
use rig_core::message::ToolResultContent;
use rig_core::tool::{Tool, ToolContext, ToolErrorKind, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::host::Run;
use crate::projects::{self, Consent, Projects, Report};
use crate::protocol::{Ask, Call, Decision};

const GUIDANCE: &str = "Finds out where the project the user is working in lives and whether its \
copies match, and keeps a copy on this host. `where` lists its places; `status` reports each \
copy's branch, HEAD and uncommitted changes and whether they match (it does not fetch); `adopt` \
asks the user whether this host may keep a copy of a project that has none, and clones it from \
its git remote if they agree; `record` notes a copy you made in `path` on `machine`, as the \
`project-sync` skill describes for a project with no remote. To bring copies level, follow the \
`project-sync` skill.";

pub struct ProjectTool {
    pub projects: Arc<Projects>,
    pub run: Arc<Run>,
    pub host: String,
    /// The remote client's machine, when it lends tools.
    #[allow(dead_code)]
    pub client: Option<String>,
    /// The project the run's client works in, as the registry names it.
    pub current: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Where,
    Status,
    Adopt,
    Record,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    pub action: Action,
    /// The project's name; the one the user works in when left out.
    #[serde(default)]
    pub project: Option<String>,
    /// For `record`: the machine the copy is on.
    #[serde(default)]
    pub machine: Option<String>,
    /// For `record`: the copy's directory.
    #[serde(default)]
    pub path: Option<String>,
}

fn failed(text: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::new(ToolErrorKind::Other, text.into())
}

impl ProjectTool {
    fn name(&self, args: &Args) -> Result<String, ToolExecutionError> {
        args.project
            .clone()
            .or_else(|| self.current.clone())
            .ok_or_else(|| failed("name a `project`; none is known for this conversation"))
    }

    async fn report(&self, machine: &str, path: &str) -> Result<Report, String> {
        let script = projects::report_script(path);
        if machine == self.host {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output()
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Report::parse(&String::from_utf8_lossy(&output.stdout)));
        }
        let call = Call {
            machine: machine.to_owned(),
            tool: "shell".into(),
            args: serde_json::json!({ "command": script }).to_string(),
            agent: None,
        };
        let content = self.run.call(call, None).await?;
        let text: String = content
            .iter()
            .filter_map(ToolResultContent::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Report::parse(&text))
    }

    async fn status(&self, name: &str) -> Result<String, ToolExecutionError> {
        let project = self.projects.get(name).ok_or_else(|| failed(format!("no project `{name}`")))?;
        let mut out = format!("{name}:\n");
        let mut reports = Vec::new();
        for (machine, path) in &project.locations {
            match self.report(machine, path).await {
                Ok(report) => {
                    out.push_str(&format!("- {machine} {path}: {}\n", report.describe()));
                    reports.push((machine.clone(), report));
                }
                Err(error) => out.push_str(&format!("- {machine} {path}: unreachable ({error})\n")),
            }
        }
        match reports.as_slice() {
            [(_, a), (_, b)] => out.push_str(&format!("{}\n", projects::compare(a, b))),
            [_] if project.locations.len() == 1 => out.push_str("only one copy exists\n"),
            _ => {}
        }
        Ok(out)
    }

    async fn adopt(&self, name: &str) -> Result<String, ToolExecutionError> {
        let project = self.projects.get(name).ok_or_else(|| failed(format!("no project `{name}`")))?;
        if let Some(path) = project.locations.get(&self.host) {
            return Ok(format!("the host already has `{name}` in {path}"));
        }
        match project.host_copy {
            Consent::Never => {
                return Err(failed("the user refused a copy on this host; do not ask again"));
            }
            Consent::Granted => {}
            Consent::Unasked => {
                let target = self.projects.clone_dir(name);
                let ask = Ask::Question {
                    question: format!(
                        "May I keep a copy of `{name}` on {} ({}), so I can work on it while \
                         your machine is away? Changes will be synced back through git.",
                        self.host,
                        target.display()
                    ),
                    choices: vec![
                        "Yes, clone it".into(),
                        "Not now".into(),
                        "Never for this project".into(),
                    ],
                };
                match self.run.ask(ask).await {
                    Decision::Answer(answer) if answer.starts_with("Yes") => {
                        self.save(name, |p| p.host_copy = Consent::Granted)?;
                    }
                    Decision::Answer(answer) if answer.starts_with("Never") => {
                        self.save(name, |p| p.host_copy = Consent::Never)?;
                        return Err(failed("the user said never; work on the client only"));
                    }
                    Decision::Stop => return Err(failed("stopped")),
                    _ => return Err(failed("the user declined for now; do not clone")),
                }
            }
        }
        let Some(remote) = project.remote.clone() else {
            return Ok(format!(
                "The user agreed to a host copy of `{name}`, but it has no git remote. Follow \
                 the no-remote section of the `{}` skill, then `record` the copy.",
                projects::SKILL
            ));
        };
        let target = self.projects.clone_dir(name);
        let mut command = tokio::process::Command::new("git");
        command.arg("clone");
        if let Some(branch) = &project.branch {
            command.args(["--branch", branch]);
        }
        let output = command
            .arg(&remote)
            .arg(&target)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|error| failed(error.to_string()))?;
        if !output.status.success() {
            return Err(failed(format!(
                "git clone failed (the host may lack access to the remote): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let path = target.display().to_string();
        let host = self.host.clone();
        self.save(name, |p| {
            p.locations.insert(host, path.clone());
        })?;
        Ok(format!("cloned `{name}` to {path} on {}", self.host))
    }

    fn save(&self, name: &str, change: impl FnOnce(&mut projects::Project)) -> Result<(), ToolExecutionError> {
        self.projects
            .update(name, change)
            .map_err(|error| failed(format!("{error:#}")))?;
        Ok(())
    }
}

impl Tool for ProjectTool {
    const NAME: &'static str = projects::TOOL;
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
        ToolAnnotations::default().with_open_world(true)
    }

    async fn call(&self, _context: &mut ToolContext, args: Args) -> Result<String, ToolExecutionError> {
        match args.action {
            Action::Where => {
                let name = self.name(&args)?;
                let project = self.projects.get(&name).ok_or_else(|| failed(format!("no project `{name}`")))?;
                let mut out = format!("{name} (host copy: {:?})\n", project.host_copy);
                if let Some(remote) = &project.remote {
                    out.push_str(&format!("git remote: {remote}\n"));
                }
                for (machine, path) in &project.locations {
                    out.push_str(&format!("- {machine}: {path}\n"));
                }
                Ok(out)
            }
            Action::Status => self.status(&self.name(&args)?).await,
            Action::Adopt => self.adopt(&self.name(&args)?).await,
            Action::Record => {
                let name = self.name(&args)?;
                let (Some(machine), Some(path)) = (args.machine, args.path) else {
                    return Err(failed("`record` needs `machine` and `path`"));
                };
                if machine == self.host && !Path::new(&path).is_absolute() {
                    return Err(failed("`path` must be absolute"));
                }
                self.save(&name, |p| {
                    p.locations.insert(machine.clone(), path.clone());
                })?;
                Ok(format!("recorded `{name}` on {machine}"))
            }
        }
    }
}
