/* -------------------------------------------------------------------------- */
/*                            subagents/access.rs                             */
/* -------------------------------------------------------------------------- */

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use toml_edit::Array;

use crate::config::{self, Directories};

/// This machine's name: `HEY_MACHINE`, else its host name without the
/// domain, as `hostname -s` prints it. Rules in a config shared between
/// machines are kept apart by it.
pub fn machine_name() -> String {
    if let Some(name) = std::env::var("HEY_MACHINE").ok().filter(|name| !name.is_empty()) {
        return name;
    }
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    match host.split('.').next() {
        Some(short) if !short.is_empty() => short.to_owned(),
        _ => "localhost".into(),
    }
}

/// Where subagents may work on this machine, as its config says and as
/// the user has since allowed.
#[derive(Debug)]
pub struct Access {
    machine: String,
    rules: Mutex<Rules>,
    /// The config file a directory always allowed is saved to.
    file: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Rules {
    allow: Vec<PathBuf>,
    exclude: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// Inside this excluded directory.
    Excluded(PathBuf),
    /// Neither: the user decides.
    Ask,
}

impl Access {
    pub fn new(machine: String, directories: Option<&Directories>, file: Option<PathBuf>) -> Result<Self> {
        let directories = directories.cloned().unwrap_or_default();
        let paths = |rules: &[String]| -> Result<Vec<PathBuf>> {
            rules.iter().map(|rule| rule_path(rule)).collect()
        };
        let rules = Rules {
            allow: paths(&directories.allow)
                .with_context(|| format!("in `subagents.machines.{machine}.allow`"))?,
            exclude: paths(&directories.exclude)
                .with_context(|| format!("in `subagents.machines.{machine}.exclude`"))?,
        };
        Ok(Self {
            machine,
            rules: Mutex::new(rules),
            file,
        })
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// Whether subagents may work in `dir`, a canonical path, given the
    /// directories this conversation already `granted`. Exclusion wins.
    pub fn verdict(&self, dir: &Path, granted: &[PathBuf]) -> Verdict {
        let rules = self.lock();
        if let Some(excluded) = rules.exclude.iter().find(|excluded| dir.starts_with(excluded)) {
            return Verdict::Excluded(excluded.clone());
        }
        match rules.allow.iter().chain(granted).any(|allowed| dir.starts_with(allowed)) {
            true => Verdict::Allowed,
            false => Verdict::Ask,
        }
    }

    /// The directories subagents are kept out of, even within their own.
    pub fn excluded(&self) -> Vec<PathBuf> {
        self.lock().exclude.clone()
    }

    /// Allows `dir` from now on, saving it to the config file's rules for
    /// this machine.
    pub fn allow_always(&self, dir: &Path) -> Result<()> {
        let mut rules = self.lock();
        let file = self
            .file
            .as_deref()
            .context("there is no config file to save it to")?;
        save_allowed(file, &self.machine, dir)
            .with_context(|| format!("cannot save it to {}", file.display()))?;
        rules.allow.push(dir.to_path_buf());
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Rules> {
        self.rules.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A configured directory, expanded and, if it exists, canonical, so it
/// compares with the canonical directories subagents ask for.
fn rule_path(rule: &str) -> Result<PathBuf> {
    let path = config::expand_path(rule)?;
    Ok(path.canonicalize().unwrap_or(path))
}

/// Adds `dir` to `subagents.machines.<machine>.allow` in the config at
/// `file`.
fn save_allowed(file: &Path, machine: &str, dir: &Path) -> Result<()> {
    config::edit(file, |document| {
        let allow = &mut document["subagents"]["machines"][machine]["allow"];
        if allow.is_none() {
            *allow = toml_edit::value(Array::new());
        }
        allow
            .as_array_mut()
            .context("its `allow` is not a list")?
            .push(shown(dir));
        Ok(())
    })
}

/// `dir` as a person would write it, under `~` when in the home directory.
fn shown(dir: &Path) -> String {
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    match home.and_then(|home| dir.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(below) if below.as_os_str().is_empty() => "~".into(),
        Some(below) => format!("~/{}", below.display()),
        None => dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn access(allow: &[&str], exclude: &[&str], file: Option<PathBuf>) -> Access {
        let directories = Directories {
            allow: allow.iter().map(|rule| rule.to_string()).collect(),
            exclude: exclude.iter().map(|rule| rule.to_string()).collect(),
        };
        Access::new("box".into(), Some(&directories), file).unwrap()
    }

    #[test]
    fn exclusion_wins_then_allowed_or_granted_directories_and_their_insides() {
        let access = access(&["/srv"], &["/srv/secrets"], None);
        let granted = [PathBuf::from("/home/me/project")];
        let verdict = |dir: &str| access.verdict(Path::new(dir), &granted);
        assert_eq!(verdict("/srv"), Verdict::Allowed);
        assert_eq!(verdict("/srv/app/src"), Verdict::Allowed);
        assert_eq!(verdict("/srv/secrets/keys"), Verdict::Excluded("/srv/secrets".into()));
        assert_eq!(verdict("/srv-other"), Verdict::Ask, "a sibling sharing a prefix is not inside");
        assert_eq!(verdict("/home/me/project/src"), Verdict::Allowed);
        assert_eq!(verdict("/home/me"), Verdict::Ask);
    }

    #[test]
    fn always_allowed_directories_are_saved_under_the_machine_keeping_the_file() {
        let dir = std::env::temp_dir().join(format!("hey-access-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hey.toml");
        fs::write(&file, "# mine\nmodel = \"openai/gpt-5.5\"\n\n[subagents.machines.box]\nexclude = [\"/srv/secrets\"]\n").unwrap();
        let access = access(&[], &["/srv/secrets"], Some(file.clone()));

        access.allow_always(Path::new("/srv/app")).unwrap();
        access.allow_always(Path::new("/opt/data")).unwrap();
        assert_eq!(access.verdict(Path::new("/srv/app/src"), &[]), Verdict::Allowed);

        let saved = fs::read_to_string(&file).unwrap();
        assert!(saved.starts_with("# mine\n"), "{saved}");
        let config: crate::config::Config = toml::from_str(&saved).unwrap();
        let rules = &config.subagents.machines["box"];
        assert_eq!(rules.allow, ["/srv/app", "/opt/data"]);
        assert_eq!(rules.exclude, ["/srv/secrets"]);
        fs::remove_dir_all(dir).unwrap();
    }
}
