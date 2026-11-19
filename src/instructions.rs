/* -------------------------------------------------------------------------- */
/*                              instructions.rs                               */
/* -------------------------------------------------------------------------- */

//! Project instructions: the `AGENTS.md` files (https://agents.md) other
//! agents share, telling the model how a project is built, tested and
//! written. They are read when a conversation starts and kept for the whole
//! of it, so the preamble stays the same and the provider's prompt cache
//! keeps working.

use std::fs;
use std::path::{Path, PathBuf};

/// The file read in each directory.
const FILE: &str = "AGENTS.md";

/// Read instead in a directory with no `AGENTS.md`.
const FALLBACK: &str = "CLAUDE.md";

/// Where the instructions for every project are kept, under the home
/// directory.
const GLOBAL: &str = ".agents/AGENTS.md";

/// What `/init` asks the model to do.
pub const INIT_PROMPT: &str = "Explore this repository and write an AGENTS.md at its root for \
coding agents that will work in it, or update the one there. Read the README, the build and \
package files, the CI configuration and a sample of the source and tests first. Keep it short and \
specific to this project: what it is and how it is laid out, the commands to build, test, lint \
and run it, its conventions for code, comments, tests and commit messages, and anything an agent \
would get wrong without being told. Leave out what any agent knows already and what the code \
makes obvious. When an AGENTS.md exists, keep what is still true, correct what is not and add \
what is missing.";

/// The instructions found for a conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// The files read, farthest first.
    pub files: Vec<PathBuf>,
    /// Their contents, each under its path, cut to the cap.
    pub text: String,
}

impl Instructions {
    /// The instructions for an agent in `dir`: `~/.agents/AGENTS.md`, then
    /// each directory's `AGENTS.md`, or its `CLAUDE.md`, from the
    /// repository root down to `dir`, at most `max_bytes` of them.
    pub fn find(dir: &Path, max_bytes: usize) -> Self {
        let global = dirs::home_dir().map(|home| home.join(GLOBAL));
        Self::read(global.into_iter().chain(project(dir)), max_bytes)
    }

    fn read(candidates: impl IntoIterator<Item = PathBuf>, max_bytes: usize) -> Self {
        let mut found = Self::default();
        for file in candidates {
            let Ok(content) = fs::read_to_string(&file) else {
                continue;
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            found.text.push_str(&format!(
                "\n\n<instructions path=\"{}\">\n{content}\n</instructions>",
                file.display()
            ));
            found.files.push(file);
        }
        if found.text.len() > max_bytes {
            let mut end = max_bytes;
            while !found.text.is_char_boundary(end) {
                end -= 1;
            }
            found.text.truncate(end);
            found.text.push_str(&format!(
                "\n[cut at {max_bytes} bytes; read the files for the rest]"
            ));
        }
        found
    }

    /// What the model is told of them, or `None` when there are none.
    pub fn preamble(&self) -> Option<String> {
        (!self.files.is_empty()).then(|| {
            format!(
                "\nProject instructions, from AGENTS.md files, farthest first; where they \
                 disagree, the nearer wins, and the user's own words win over all:{}",
                self.text
            )
        })
    }
}

/// The file each directory from the repository root down to `dir` offers:
/// its `AGENTS.md`, else its `CLAUDE.md`. Without a repository, only `dir`'s.
fn project(dir: &Path) -> Vec<PathBuf> {
    let ancestors: Vec<&Path> = dir.ancestors().collect();
    let repository = ancestors
        .iter()
        .position(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(0);
    ancestors[..=repository]
        .iter()
        .rev()
        .filter_map(|ancestor| {
            [FILE, FALLBACK]
                .into_iter()
                .map(|name| ancestor.join(name))
                .find(|file| file.is_file())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own for a test, removed once done.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("hey-instructions-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_from_the_repository_root_down_preferring_agents_md() {
        let root = Scratch::new("repo");
        let outside = root.path();
        let repo = outside.join("repo");
        let deep = repo.join("crates/deep");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(outside.join("AGENTS.md"), "outside the repository").unwrap();
        fs::write(repo.join("AGENTS.md"), "root").unwrap();
        fs::write(repo.join("CLAUDE.md"), "shadowed").unwrap();
        fs::write(repo.join("crates/CLAUDE.md"), "fallback").unwrap();
        fs::write(deep.join("AGENTS.md"), "  \n").unwrap();

        let found = Instructions::read(project(&deep), 1 << 15);
        assert_eq!(
            found.files,
            [repo.join("AGENTS.md"), repo.join("crates/CLAUDE.md")]
        );
        assert!(found.text.find("root").unwrap() < found.text.find("fallback").unwrap());
        assert!(!found.text.contains("shadowed"));
        assert!(!found.text.contains("outside"));
        assert!(found.preamble().unwrap().contains("fallback"));
    }

    #[test]
    fn without_a_repository_reads_only_the_working_directory() {
        let root = Scratch::new("plain");
        let dir = root.path().join("work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(root.path().join("AGENTS.md"), "above").unwrap();
        fs::write(dir.join("AGENTS.md"), "here").unwrap();
        assert_eq!(project(&dir), [dir.join("AGENTS.md")]);
    }

    #[test]
    fn cuts_at_the_cap_on_a_character_boundary() {
        let root = Scratch::new("cap");
        let file = root.path().join("AGENTS.md");
        fs::write(&file, "é".repeat(100)).unwrap();
        let found = Instructions::read([file], 101);
        assert!(
            found
                .text
                .ends_with("[cut at 101 bytes; read the files for the rest]")
        );
        assert!(found.text.len() < 101 + 60);
        assert_eq!(Instructions::read([], 10).preamble(), None);
    }
}
