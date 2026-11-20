/* -------------------------------------------------------------------------- */
/*                              subagents/kinds.rs                            */
/* -------------------------------------------------------------------------- */

//! Named kinds of subagent, each a Markdown file whose front matter names
//! and describes it and may give it a model, its tools and how much it
//! thinks, and whose body is its system prompt. The main agent starts one
//! by giving its name as a task's `type`. They are found where skills are,
//! under `agents` instead of `skills`, and read afresh for every run.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Thinking;
use crate::skills::{self, Name, Source};

/// Where kinds are kept, under the home directory or a project's.
const ROOT: &str = ".agents/agents";

const MAX_DESCRIPTION: usize = 1024;

/// A kind of subagent, as its file describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kind {
    pub name: Name,
    pub description: String,
    pub source: Source,
    pub file: PathBuf,
    /// As `vendor/model`; unset, the one subagents use otherwise.
    pub model: Option<String>,
    /// The only tools it has, by name; unset, every subagent tool.
    pub tools: Option<Vec<String>>,
    pub thinking: Option<Thinking>,
    /// Its system prompt, in place of the generic subagent one.
    pub prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    thinking: Option<Thinking>,
}

impl Kind {
    /// The kind `file` describes, or why it does not.
    fn load(file: PathBuf, source: Source) -> Result<Self, String> {
        let described = fs::read_to_string(&file)
            .map_err(|error| error.to_string())
            .and_then(|text| Self::describe(&text, file.clone(), source));
        described.map_err(|why| format!("{}: {why}", file.display()))
    }

    fn describe(text: &str, file: PathBuf, source: Source) -> Result<Self, String> {
        let (yaml, body) = skills::split(text).ok_or("no front matter between `---` lines")?;
        let front: Frontmatter =
            serde_yaml_ng::from_str(yaml).map_err(|error| format!("front matter: {error}"))?;
        let name = Name::try_from(front.name)?;
        if file.file_stem().and_then(|stem| stem.to_str()) != Some(name.as_str()) {
            return Err(format!("name `{name}` differs from its file's"));
        }
        let description = front.description.trim().to_owned();
        if description.is_empty() || description.chars().count() > MAX_DESCRIPTION {
            return Err(format!(
                "description must be 1 to {MAX_DESCRIPTION} characters"
            ));
        }
        let prompt = body.trim().to_owned();
        if prompt.is_empty() {
            return Err("no system prompt after the front matter".into());
        }
        Ok(Self {
            name,
            description,
            source,
            file,
            model: front.model.filter(|model| !model.trim().is_empty()),
            tools: front.tools,
            thinking: front.thinking,
            prompt,
        })
    }

    /// Whether it may have the tool named `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|allowed| allowed == tool))
    }
}

/// The kinds found for an agent working in one directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Kinds {
    /// By name, each the one nearest the directory.
    pub kinds: Vec<Kind>,
    /// Why each file found was not loaded.
    pub problems: Vec<String>,
    /// Where they were looked for, farthest first.
    pub roots: Vec<(Source, PathBuf)>,
}

impl Kinds {
    pub fn get(&self, name: &str) -> Option<&Kind> {
        self.kinds.iter().find(|kind| kind.name.as_str() == name)
    }
}

/// The kinds for an agent working in `dir`, from `~/.agents/agents` and the
/// project's `.agents/agents` as for skills: the nearest of a name wins.
pub fn find(dir: &Path) -> Kinds {
    find_in(dirs::home_dir().as_deref(), dir)
}

/// As [`find`], with `home` as the home directory.
fn find_in(home: Option<&Path>, dir: &Path) -> Kinds {
    let roots = skills::roots_in(home, dir, ROOT);
    let mut kinds = BTreeMap::new();
    let mut problems = Vec::new();
    for (source, root) in &roots {
        for loaded in scan(root, *source) {
            match loaded {
                Ok(kind) => {
                    kinds.insert(kind.name.clone(), kind);
                }
                Err(problem) => problems.push(problem),
            }
        }
    }
    Kinds {
        kinds: kinds.into_values().collect(),
        problems,
        roots,
    }
}

/// The `.md` files directly inside `root`, in name order.
fn scan(root: &Path, source: Source) -> Vec<Result<Kind, String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![Err(format!("{}: {error}", root.display()))],
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|file| file.extension().is_some_and(|ext| ext == "md") && file.is_file())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|file| Kind::load(file, source))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(text: &str, stem: &str) -> Result<Kind, String> {
        Kind::describe(
            text,
            PathBuf::from(format!("/agents/{stem}.md")),
            Source::Global,
        )
    }

    #[test]
    fn reads_front_matter_and_takes_the_body_as_its_prompt() {
        let text = "---\nname: reviewer\ndescription: Reviews changes.\nmodel: openai/gpt-5.4\n\
                    tools: [read, search]\nthinking: high\n---\n\nYou review.\n";
        let kind = describe(text, "reviewer").unwrap();
        assert_eq!(kind.prompt, "You review.");
        assert_eq!(kind.model.as_deref(), Some("openai/gpt-5.4"));
        assert_eq!(kind.thinking, Some(Thinking::High));
        assert!(kind.allows("read") && !kind.allows("shell"));

        let open = describe("---\nname: any\ndescription: d\n---\nGo.", "any").unwrap();
        assert!(open.allows("shell") && open.model.is_none());

        let problem = |text: &str, stem| describe(text, stem).unwrap_err();
        assert!(problem("---\nname: a\ndescription: d\n---\nGo.", "b").contains("file"));
        assert!(problem("---\nname: a\ndescription: d\n---\n", "a").contains("system prompt"));
        assert!(
            problem("---\nname: a\ndescription: d\ncolor: red\n---\nGo.", "a").contains("color")
        );
        assert!(
            problem(
                "---\nname: a\ndescription: d\nthinking: lots\n---\nGo.",
                "a"
            )
            .contains("front matter")
        );
        assert!(problem("Go.", "a").contains("front matter"));
    }

    #[test]
    fn the_kind_nearest_the_directory_wins() {
        let base = std::env::temp_dir().join(format!("hey-kinds-{}", std::process::id()));
        let repo = base.join("repo");
        let inner = repo.join("inner");
        let write = |root: &Path, name: &str, description: &str| {
            let dir = root.join(ROOT);
            fs::create_dir_all(&dir).unwrap();
            let text = format!("---\nname: {name}\ndescription: {description}\n---\nWork.");
            fs::write(dir.join(format!("{name}.md")), text).unwrap();
        };
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&inner).unwrap();
        write(&repo, "reviewer", "the repository's");
        write(&repo, "tester", "the repository's");
        write(&inner, "reviewer", "the directory's");
        fs::write(inner.join(ROOT).join("Bad.md"), "---\nname: Bad\n---").unwrap();
        fs::write(inner.join(ROOT).join("notes.txt"), "ignored").unwrap();

        let found = find_in(Some(&base.join("home")), &inner);
        assert_eq!(found.kinds.len(), 2);
        assert_eq!(
            found.get("reviewer").unwrap().description,
            "the directory's"
        );
        assert_eq!(found.get("tester").unwrap().source, Source::Project);
        assert_eq!(found.problems.len(), 1);
        assert!(found.problems[0].contains("Bad.md"));
        fs::remove_dir_all(base).unwrap();
    }
}
