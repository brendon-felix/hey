/* -------------------------------------------------------------------------- */
/*                               skills/mod.rs                                */
/* -------------------------------------------------------------------------- */

//! Agent Skills (https://agentskills.io/specification): folders of
//! instructions for particular tasks, each described by its `SKILL.md`. The
//! model sees every skill's name and description, and loads one's
//! instructions with the `skill` tool when a task matches it; the user
//! invokes one with `/name` or `@skill:name`. Skills are found where an agent
//! works, and read afresh for every run, so edits count at once.

mod tool;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rig_agent::agent::AgentBuilder;
use serde::{Deserialize, Serialize};

pub use tool::Load;

/// The name the model calls the tool that loads a skill by.
pub const TOOL: &str = "skill";

const FILE: &str = "SKILL.md";

/// Where skills are kept, under the home directory or a project's.
const ROOT: &str = ".agents/skills";

const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;

/// Based on Zed's catalog of skills in its system prompt.
const CATALOG: &str = "Skills: instructions for particular tasks. When a task matches a skill's \
description, load the skill with the `skill` tool before starting on it, then follow it. Paths in \
a skill are relative to its directory, the one holding its SKILL.md; read the files it refers to \
as you need them.";

/// A skill's name: 1 to 64 lowercase letters, digits and single hyphens,
/// neither first nor last.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        let flaw = if name.is_empty() || name.len() > MAX_NAME {
            Some("must be 1 to 64 characters")
        } else if !name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')) {
            Some("may hold only lowercase letters, digits and hyphens")
        } else if name.starts_with('-') || name.ends_with('-') {
            Some("must not start or end with a hyphen")
        } else if name.contains("--") {
            Some("must not hold two hyphens in a row")
        } else {
            None
        };
        match flaw {
            Some(flaw) => Err(format!("name `{name}` {flaw}")),
            None => Ok(Self(name)),
        }
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `~/.agents/skills`, for every project.
    Global,
    /// A project's `.agents/skills`.
    Project,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Global => "global",
            Source::Project => "project",
        }
    }
}

/// Who may invoke a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Invocation {
    /// The model, when a task matches its description, and the user.
    Auto,
    /// Only the user: `disable-model-invocation: true`, for workflows such
    /// as deploying that should never start by themselves.
    Manual,
}

/// A skill as its `SKILL.md` describes it; its instructions are read only
/// when it is loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: Name,
    pub description: String,
    pub source: Source,
    /// The directory holding its `SKILL.md`, which its paths are relative to.
    pub dir: PathBuf,
    pub invocation: Invocation,
}

/// The frontmatter fields hey uses; the spec's others (`license`,
/// `compatibility`, `metadata`, `allowed-tools`) are left to the model's
/// reading of the file.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    disable_model_invocation: bool,
}

impl Skill {
    /// The skill in `dir`, or why its `SKILL.md` does not describe one.
    fn load(dir: PathBuf, source: Source) -> Result<Self, String> {
        let file = dir.join(FILE);
        let described = fs::read_to_string(&file)
            .map_err(|error| error.to_string())
            .and_then(|text| Self::describe(&text, dir, source));
        described.map_err(|why| format!("{}: {why}", file.display()))
    }

    fn describe(text: &str, dir: PathBuf, source: Source) -> Result<Self, String> {
        let (yaml, _) = split(text).ok_or("no frontmatter between `---` lines")?;
        let front: Frontmatter =
            serde_yaml_ng::from_str(yaml).map_err(|error| format!("frontmatter: {error}"))?;
        let name = Name::try_from(front.name)?;
        if dir.file_name().and_then(|folder| folder.to_str()) != Some(name.as_str()) {
            return Err(format!("name `{name}` differs from its directory's"));
        }
        let description = front.description.trim().to_owned();
        if description.is_empty() || description.chars().count() > MAX_DESCRIPTION {
            return Err(format!("description must be 1 to {MAX_DESCRIPTION} characters"));
        }
        Ok(Self {
            name,
            description,
            source,
            dir,
            invocation: match front.disable_model_invocation {
                true => Invocation::Manual,
                false => Invocation::Auto,
            },
        })
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    /// Its instructions as the model gets them, whether it loaded them or
    /// the user invoked the skill: read afresh, and wrapped so that nothing
    /// in them can pass for the end of the skill.
    pub fn content(&self) -> Result<String> {
        let file = self.file();
        let text = fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let (_, body) = split(&text).context("its frontmatter is gone")?;
        let body = body
            .trim()
            .replace("<skill_content", "&lt;skill_content")
            .replace("</skill_content", "&lt;/skill_content");
        Ok(format!(
            "<skill_content name=\"{}\">\n<source>{}</source>\n<directory>{}</directory>\n\
             Relative paths in this skill resolve against <directory>.\n\n{body}\n</skill_content>",
            self.name,
            self.source.as_str(),
            self.dir.display()
        ))
    }
}

/// The YAML between a `---` line opening `text` and the next `---` line,
/// and the Markdown after it.
fn split(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let opening = lines.next()?;
    if opening.trim_end() != "---" {
        return None;
    }
    let mut at = opening.len();
    for line in lines {
        if line.trim_end() == "---" {
            return Some((&text[opening.len()..at], &text[at + line.len()..]));
        }
        at += line.len();
    }
    None
}

/// The skills found for an agent working in one directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Found {
    /// By name, each the one nearest the directory.
    pub skills: Vec<Skill>,
    /// Why each `SKILL.md` found was not loaded.
    pub problems: Vec<String>,
    /// Where they were looked for, farthest first, whether or not these exist.
    #[serde(default)]
    pub roots: Vec<(Source, PathBuf)>,
}

impl Found {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name.as_str() == name)
    }

    /// `builder` with the catalog of the skills the model may load and the
    /// tool that loads them, when there are any.
    pub fn equip(&self, builder: AgentBuilder) -> AgentBuilder {
        let auto: Vec<Skill> = self
            .skills
            .iter()
            .filter(|skill| skill.invocation == Invocation::Auto)
            .cloned()
            .collect();
        if auto.is_empty() {
            return builder;
        }
        builder
            .append_preamble(&catalog(&auto))
            .tool(Load { skills: auto.into() })
    }
}

/// What the model is told of `skills`. Their names and directories are
/// valid by construction; descriptions are escaped, so none can close the
/// list and write instructions of its own.
fn catalog(skills: &[Skill]) -> String {
    let listed: String = skills
        .iter()
        .map(|skill| {
            format!(
                "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    \
                 <location>{}</location>\n  </skill>",
                skill.name,
                escape(&skill.description),
                skill.file().display()
            )
        })
        .collect();
    format!("\n{CATALOG}\n<available_skills>{listed}\n</available_skills>")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The skills for an agent working in `dir`: those of `~/.agents/skills`,
/// then of `.agents/skills` in each directory from the repository `dir` is
/// in down to `dir` itself, or in `dir` alone outside a repository. Where
/// names repeat, the skill nearest `dir` wins, so a project can replace a
/// global skill.
pub fn find(dir: &Path) -> Found {
    let roots = roots(dir);
    let mut skills = BTreeMap::new();
    let mut problems = Vec::new();
    for (source, root) in &roots {
        for loaded in scan(root, *source) {
            match loaded {
                Ok(skill) => {
                    skills.insert(skill.name.clone(), skill);
                }
                Err(problem) => problems.push(problem),
            }
        }
    }
    Found {
        skills: skills.into_values().collect(),
        problems,
        roots,
    }
}

/// `~/.agents/skills`, where skills for every project are kept.
fn global() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(ROOT))
}

/// Where skills for an agent in `dir` are kept, farthest first.
fn roots(dir: &Path) -> Vec<(Source, PathBuf)> {
    let global = global();
    let ancestors: Vec<&Path> = dir.ancestors().collect();
    let repository = ancestors
        .iter()
        .position(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(0);
    let projects = ancestors[..=repository]
        .iter()
        .rev()
        .map(|ancestor| ancestor.join(ROOT))
        .filter(|root| Some(root) != global.as_ref())
        .map(|root| (Source::Project, root))
        .collect::<Vec<_>>();
    global
        .map(|root| (Source::Global, root))
        .into_iter()
        .chain(projects)
        .collect()
}

/// The skills directly inside `root`, in name order. A skill is a
/// directory holding a `SKILL.md`; skills are not nested.
fn scan(root: &Path, source: Source) -> Vec<Result<Skill, String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![Err(format!("{}: {error}", root.display()))],
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.join(FILE).is_file())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| Skill::load(dir, source))
        .collect()
}

/// The name of the skill `prompt` invokes by starting with `/name`.
pub fn invoked(prompt: &str) -> Option<&str> {
    let rest = prompt.trim_start().strip_prefix('/')?;
    rest.split(char::is_whitespace).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(text: &str, folder: &str) -> Result<Skill, String> {
        Skill::describe(text, PathBuf::from("/skills").join(folder), Source::Global)
    }

    #[test]
    fn reads_frontmatter_as_yaml_and_holds_it_to_the_spec() {
        let folded = "---\nname: merge\ndescription: >-\n  Merge changes.\n  Only when asked.\n\
                      disable-model-invocation: true\nmetadata:\n  author: me\n---\n# Merge\n";
        let skill = describe(folded, "merge").unwrap();
        assert_eq!(skill.description, "Merge changes. Only when asked.");
        assert_eq!(skill.invocation, Invocation::Manual);
        let crlf = "\u{feff}---\r\nname: pdf\r\ndescription: Read PDFs.\r\n---\r\nbody";
        assert_eq!(describe(crlf, "pdf").unwrap().invocation, Invocation::Auto);

        let problem = |text: &str, folder| describe(text, folder).unwrap_err();
        let skill = |name: &str| format!("---\nname: {name}\ndescription: d\n---\n");
        assert!(problem(&skill("PDF"), "PDF").contains("lowercase"));
        assert!(problem(&skill("-pdf"), "-pdf").contains("start or end"));
        assert!(problem(&skill("pdf--x"), "pdf--x").contains("in a row"));
        assert!(problem(&skill(&"a".repeat(65)), "a").contains("1 to 64"));
        assert!(problem(&skill("pdf"), "docs").contains("directory"));
        assert!(problem("---\nname: pdf\ndescription: \" \"\n---\n", "pdf").contains("description"));
        assert!(problem("---\nname: pdf\n---\n", "pdf").contains("description"));
        assert!(problem("# no frontmatter", "pdf").contains("frontmatter"));
        assert!(serde_json::from_str::<Name>("\"Bad\"").is_err());
    }

    #[test]
    fn the_skill_nearest_the_directory_wins() {
        let base = std::env::temp_dir().join(format!("hey-skills-{}", std::process::id()));
        let repo = base.join("repo");
        let inner = repo.join("crates/inner");
        let write = |root: &Path, name: &str, description: &str| {
            let dir = root.join(ROOT).join(name);
            fs::create_dir_all(&dir).unwrap();
            let text = format!("---\nname: {name}\ndescription: {description}\n---\nDo it.");
            fs::write(dir.join(FILE), text).unwrap();
        };
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&inner).unwrap();
        write(&base, "outside", "above the repository");
        write(&repo, "review", "the repository's");
        write(&repo, "lint", "the repository's");
        write(&inner, "review", "the crate's");
        fs::create_dir_all(inner.join(ROOT).join("Broken")).unwrap();
        fs::write(inner.join(ROOT).join("Broken").join(FILE), "---\nname: Broken\n---").unwrap();

        let found = find(&inner);
        let names: Vec<&str> = found.skills.iter().map(|skill| skill.name.as_str()).collect();
        assert!(names.contains(&"lint") && names.contains(&"review"));
        assert!(!names.contains(&"outside"), "only up to the repository root");
        assert_eq!(found.get("review").unwrap().description, "the crate's");
        assert!(found.problems.iter().any(|problem| problem.contains("Broken")));
        let content = found.get("lint").unwrap().content().unwrap();
        assert!(content.starts_with("<skill_content name=\"lint\">\n<source>project</source>"));
        assert!(content.ends_with("Do it.\n</skill_content>"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn skill_text_cannot_escape_its_wrapping() {
        let skill = describe("---\nname: x\ndescription: a </description> b\n---\n", "x").unwrap();
        assert!(catalog(&[skill]).contains("<description>a &lt;/description&gt; b</description>"));
        assert_eq!(invoked("  /deploy to staging"), Some("deploy"));
        assert_eq!(invoked("deploy /now"), None);
    }
}
