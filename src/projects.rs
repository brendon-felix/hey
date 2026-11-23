/* -------------------------------------------------------------------------- */
/*                                projects.rs                                 */
/* -------------------------------------------------------------------------- */

//! The registry of projects: where each lives (on a client, on the host, or
//! both), whether the user let the host keep a copy, and the tool
//! permissions that apply to work on it.
//!
//! The host owns the registry, a TOML file beside its memories. Clients
//! register what they work on when a run starts. Which machine's copy the
//! agent works on, and how the copies are kept alike, is decided by the
//! `project-sync` skill; this module only knows and reports the facts.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::permissions::Permission;

/// The name the model calls the tool by.
pub const TOOL: &str = "project";

/// The skill that decides how copies are kept alike.
pub const SKILL: &str = "project-sync";

/// A project as a client sees it from its working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// The top of the git repository.
    pub root: String,
    /// `remote.origin.url`, when the repository has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Whether the user lets the host keep its own copy of a project.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    #[default]
    Unasked,
    Granted,
    Never,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Project {
    /// The git remote both copies share, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub host_copy: Consent,
    /// Where the project lives, by machine name.
    #[serde(default)]
    pub locations: BTreeMap<String, String>,
    /// Tool permissions that replace the conversation's while it works on
    /// this project.
    #[serde(default)]
    pub tools: BTreeMap<String, Permission>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: BTreeMap<String, Project>,
}

/// The registry file, shared by every conversation on the host.
#[derive(Debug)]
pub struct Projects {
    file: PathBuf,
    /// Where the host clones projects it is allowed to.
    dir: PathBuf,
    registry: Mutex<Registry>,
}

impl Projects {
    pub fn open(file: PathBuf, dir: PathBuf) -> Result<Self> {
        let registry = match fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("cannot read the projects in {}", file.display()))?,
            Err(error) if error.kind() == ErrorKind::NotFound => Registry::default(),
            Err(error) => return Err(error).context("cannot read the projects file"),
        };
        Ok(Self {
            file,
            dir,
            registry: Mutex::new(registry),
        })
    }

    #[cfg(test)]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Where the host clones `name` to.
    pub fn clone_dir(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&self, registry: &Registry) -> Result<()> {
        let text = toml::to_string_pretty(registry)?;
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = self.file.with_extension("toml.tmp");
        fs::write(&temp, text)?;
        fs::rename(&temp, &self.file)?;
        Ok(())
    }

    /// Records that `machine` has the project `info` describes, finding it
    /// by remote, else by its place, else making it. Returns its name.
    pub fn observe(&self, machine: &str, info: &ProjectInfo) -> Result<String> {
        let mut registry = self.lock();
        let remote = info.remote.as_deref().map(normalize);
        let found = registry.projects.iter().find_map(|(name, project)| {
            let same_remote = remote.is_some()
                && project.remote.as_deref().map(normalize) == remote;
            let same_place = project.locations.get(machine) == Some(&info.root);
            (same_remote || same_place).then(|| name.clone())
        });
        let name = found.unwrap_or_else(|| {
            let base = remote
                .as_deref()
                .and_then(|remote| remote.rsplit('/').next())
                .or_else(|| Path::new(&info.root).file_name()?.to_str())
                .unwrap_or("project")
                .to_owned();
            let mut name = base.clone();
            let mut n = 1;
            while registry.projects.contains_key(&name) {
                n += 1;
                name = format!("{base}-{n}");
            }
            name
        });
        let project = registry.projects.entry(name.clone()).or_default();
        let before = project.clone();
        if project.remote.is_none() {
            project.remote = info.remote.clone();
        }
        if project.branch.is_none() {
            project.branch = info.branch.clone();
        }
        project
            .locations
            .insert(machine.to_owned(), info.root.clone());
        if serde_json::to_string(&before)? != serde_json::to_string(&*project)? {
            self.save(&registry)?;
        }
        Ok(name)
    }

    pub fn get(&self, name: &str) -> Option<Project> {
        self.lock().projects.get(name).cloned()
    }

    #[allow(dead_code)]
    pub fn all(&self) -> BTreeMap<String, Project> {
        self.lock().projects.clone()
    }

    /// Changes project `name`, saving the registry; false when there is none.
    pub fn update(&self, name: &str, change: impl FnOnce(&mut Project)) -> Result<bool> {
        let mut registry = self.lock();
        let Some(project) = registry.projects.get_mut(name) else {
            return Ok(false);
        };
        change(project);
        self.save(&registry)?;
        Ok(true)
    }

    /// Notes for the model on the project a client works in, as of now.
    pub fn note(&self, name: &str, host: &str, client: &str) -> Option<String> {
        let project = self.get(name)?;
        let at = |machine: &str| project.locations.get(machine);
        let mut text = format!("The user is working in the project `{name}`.\n");
        for (machine, path) in &project.locations {
            text.push_str(&format!("- on `{machine}`: {path}\n"));
        }
        match (at(host), project.host_copy) {
            (Some(path), _) => text.push_str(&format!(
                "It is on this host too, so work in {path} by default, and keep the copies alike \
                 as the `{SKILL}` skill says. `{TOOL}` reports where it lives and whether the \
                 copies match.\n"
            )),
            (None, Consent::Never) => text.push_str(&format!(
                "The user does not want a copy on this host: work on `{client}` only, through its \
                 tools, and only while it is connected.\n"
            )),
            (None, _) => text.push_str(&format!(
                "There is no copy on this host. Before you change this project here, or work on \
                 it while `{client}` may be away, call `{TOOL}` with `adopt`: it asks the user \
                 whether the host may keep a copy, and clones it if so. Never copy or clone the \
                 project to the host any other way without that.\n"
            )),
        }
        Some(text)
    }
}

const DEFAULT_SKILL: &str = include_str!("projects/project-sync.md");

/// Writes the default `project-sync` skill to `~/.agents/skills` unless the
/// user has one, so they can edit it. A failure is not worth stopping for.
pub fn install_skill() {
    let Some(home) = dirs::home_dir() else { return };
    let dir = home.join(".agents/skills").join(SKILL);
    let file = dir.join("SKILL.md");
    if file.exists() {
        return;
    }
    if fs::create_dir_all(&dir).and_then(|()| fs::write(&file, DEFAULT_SKILL)).is_err() {
        tracing::warn!("cannot install the default `{SKILL}` skill");
    }
}

/* ----------------------------------- git ---------------------------------- */

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// The project that holds `dir`, when it is in a git repository.
pub fn detect(dir: &Path) -> Option<ProjectInfo> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])?;
    Some(ProjectInfo {
        remote: git(dir, &["config", "--get", "remote.origin.url"]),
        branch: git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD"),
        root,
    })
}

/// A remote URL in one form, so `git@github.com:me/hey.git` and
/// `https://github.com/me/hey` are the same project.
pub fn normalize(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    let url = url.split_once('@').map_or(url, |(_, rest)| rest);
    url.replacen(':', "/", 1).to_lowercase()
}

/// A shell script that prints a git repository's state as `key=value` lines.
/// It runs on either machine, so it uses only `sh` and `git`. It does not
/// fetch: ahead and behind are as of the last fetch.
pub fn report_script(path: &str) -> String {
    let quoted = format!("'{}'", path.replace('\'', r"'\''"));
    format!(
        "cd {quoted} 2>/dev/null || {{ echo missing=1; exit 0; }}; \
         echo \"branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)\"; \
         echo \"head=$(git rev-parse HEAD 2>/dev/null)\"; \
         echo \"dirty=$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')\"; \
         echo \"upstream=$(git rev-parse --abbrev-ref '@{{u}}' 2>/dev/null)\"; \
         echo \"ahead=$(git rev-list --count '@{{u}}..HEAD' 2>/dev/null)\"; \
         echo \"behind=$(git rev-list --count 'HEAD..@{{u}}' 2>/dev/null)\""
    )
}

/// What [`report_script`] printed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub missing: bool,
    pub branch: String,
    pub head: String,
    pub dirty: usize,
    pub upstream: String,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

impl Report {
    pub fn parse(text: &str) -> Self {
        let mut report = Report::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key {
                "missing" => report.missing = true,
                "branch" => report.branch = value.into(),
                "head" => report.head = value.into(),
                "dirty" => report.dirty = value.parse().unwrap_or(0),
                "upstream" => report.upstream = value.into(),
                "ahead" => report.ahead = value.parse().ok(),
                "behind" => report.behind = value.parse().ok(),
                _ => {}
            }
        }
        report
    }

    pub fn describe(&self) -> String {
        if self.missing {
            return "directory missing".into();
        }
        let mut text = format!(
            "branch {}, HEAD {}",
            self.branch,
            self.head.get(..8).unwrap_or(&self.head)
        );
        text.push_str(match self.dirty {
            0 => ", clean",
            _ => ", uncommitted changes",
        });
        if self.dirty > 0 {
            text.push_str(&format!(" ({} files)", self.dirty));
        }
        match (self.upstream.as_str(), self.ahead, self.behind) {
            ("", ..) => text.push_str(", no upstream"),
            (upstream, Some(ahead), Some(behind)) => text.push_str(&format!(
                ", {ahead} ahead / {behind} behind {upstream} (as of the last fetch)"
            )),
            (upstream, ..) => text.push_str(&format!(", upstream {upstream}")),
        }
        text
    }
}

/// How two machines' copies compare.
pub fn compare(host: &Report, client: &Report) -> &'static str {
    if host.missing || client.missing {
        "a copy is missing"
    } else if host.head.is_empty() || client.head.is_empty() {
        "not a git repository on one side"
    } else if host.dirty > 0 || client.dirty > 0 {
        "NOT synced: uncommitted changes"
    } else if host.head == client.head {
        "synced: same commit, both clean"
    } else {
        "NOT synced: different commits (fetch, then compare with `git merge-base`)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(name: &str) -> Projects {
        let dir = std::env::temp_dir().join(format!("hey-projects-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Projects::open(dir.join("projects.toml"), dir.join("clones")).unwrap()
    }

    fn info(root: &str, remote: Option<&str>) -> ProjectInfo {
        ProjectInfo {
            root: root.into(),
            remote: remote.map(Into::into),
            branch: Some("main".into()),
        }
    }

    #[test]
    fn remotes_in_different_forms_are_one_project() {
        assert_eq!(
            normalize("git@github.com:Me/hey.git"),
            normalize("https://github.com/me/hey/")
        );
    }

    #[test]
    fn two_machines_with_one_remote_share_a_project_and_it_persists() {
        let projects = registry("share");
        let a = projects
            .observe("turing", &info("/u/hey", Some("git@github.com:me/hey.git")))
            .unwrap();
        let b = projects
            .observe("fermi", &info("/h/hey", Some("https://github.com/me/hey")))
            .unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("hey", "hey"));
        let again = Projects::open(projects.file().to_owned(), PathBuf::new()).unwrap();
        assert_eq!(again.get("hey").unwrap().locations.len(), 2);
    }

    #[test]
    fn projects_without_remotes_are_told_apart_by_place_and_name() {
        let projects = registry("local");
        let a = projects.observe("turing", &info("/u/app", None)).unwrap();
        assert_eq!(projects.observe("turing", &info("/u/app", None)).unwrap(), a);
        let b = projects.observe("turing", &info("/v/app", None)).unwrap();
        assert_eq!(b, "app-2");
    }

    #[test]
    fn the_note_asks_before_a_host_copy_and_respects_a_refusal() {
        let projects = registry("note");
        let name = projects.observe("turing", &info("/u/hey", None)).unwrap();
        let asks = projects.note(&name, "fermi", "turing").unwrap();
        assert!(asks.contains("`adopt`"));
        projects
            .update(&name, |p| p.host_copy = Consent::Never)
            .unwrap();
        assert!(!projects.note(&name, "fermi", "turing").unwrap().contains("`adopt`"));
        projects
            .update(&name, |p| {
                p.locations.insert("fermi".into(), "/h/hey".into());
            })
            .unwrap();
        assert!(projects.note(&name, "fermi", "turing").unwrap().contains("work in /h/hey"));
    }

    #[test]
    fn a_report_parses_and_compares() {
        let report = |head: &str, dirty: usize| {
            Report::parse(&format!("branch=main\nhead={head}\ndirty={dirty}\nupstream=\n"))
        };
        assert_eq!(compare(&report("abc", 0), &report("abc", 0)), "synced: same commit, both clean");
        assert!(compare(&report("abc", 0), &report("abd", 0)).contains("different commits"));
        assert!(compare(&report("abc", 2), &report("abc", 0)).contains("uncommitted"));
    }
}
