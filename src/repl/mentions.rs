/* -------------------------------------------------------------------------- */
/*                              repl/mentions.rs                              */
/* -------------------------------------------------------------------------- */

use std::cmp::Reverse;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use reedline::{Span, Suggestion};

use super::commands;

use crate::mcp;
use crate::mention::{self, Item, Reference, SHARED, SKILL, THREAD};
use crate::protocol::{AgentInfo, ModelChoice};
use crate::sessions::Summary;
use crate::skills::{Found, Skill};

/// How long a walk of the working directory, or what the host offers,
/// answers completions before it is fetched again.
pub const STALE_AFTER: Duration = Duration::from_secs(5);
const MAX_SUGGESTED: usize = 50;

/// What the host offers to mention, and the models it can use, as last
/// fetched.
#[derive(Debug, Default)]
pub struct HostIndex {
    pub threads: Vec<Summary>,
    /// The subagents of the conversation the terminal shows.
    pub agents: Vec<AgentInfo>,
    pub shared: Vec<String>,
    pub skills: Found,
    pub prompts: Vec<mcp::Prompt>,
    pub models: Vec<ModelChoice>,
}

pub type Index = Arc<Mutex<HostIndex>>;

pub fn lock(index: &Index) -> MutexGuard<'_, HostIndex> {
    index.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HostIndex {
    /// The skill `/name` invokes: none a command takes the place of.
    pub fn command(&self, name: &str) -> Option<&Skill> {
        self.skills
            .get(name)
            .filter(|_| commands::find(name).is_none())
    }

    /// The MCP prompt `/word` invokes.
    pub fn prompt(&self, word: &str) -> Option<&mcp::Prompt> {
        mcp::invoked(&self.prompts, word)
    }

    /// What `reference` attaches, for the status line; `None` when it names
    /// nothing known.
    pub fn describe(&self, reference: Reference) -> Option<String> {
        match reference {
            Reference::Local(path) => Item::local(path).map(|item| item.to_string()),
            Reference::Shared("") => None,
            Reference::Shared(path) => self
                .shared
                .iter()
                .find(|entry| entry.trim_end_matches('/') == path.trim_end_matches('/'))
                .map(|entry| format!("{SHARED}{entry}")),
            Reference::Thread(id) => self
                .threads
                .iter()
                .find(|summary| summary.id.as_str() == id)
                .map(|summary| format!("conversation {summary}")),
            Reference::Skill(name) => self
                .skills
                .get(name)
                .map(|skill| format!("{SKILL}{}", skill.name)),
        }
    }
}

/// What an `@` mention completes to: the user's working directory's files
/// and directories, then after `@shared:` the host's shared repository's,
/// after `@thread:` its saved conversations, and after `@skill:` its skills.
pub struct Mentions {
    index: Index,
    matcher: SkimMatcherV2,
    local: Cache<Vec<String>>,
}

impl Mentions {
    pub fn new(index: Index) -> Self {
        Self {
            index,
            matcher: SkimMatcherV2::default(),
            local: Cache(None),
        }
    }

    /// What the mention at `pos` in `line` completes to; the whole of it,
    /// past the cursor too, is what a suggestion replaces.
    pub fn suggestions(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let before = line.get(..pos).unwrap_or(line);
        let Some(typed) = mention::under(before, before.len()) else {
            return Vec::new();
        };
        let end = mention::under(line, pos)
            .filter(|whole| whole.span.start == typed.span.start)
            .map_or(typed.span.end, |whole| whole.span.end);
        let span = Span::new(typed.span.start, end);
        match typed.reference() {
            Reference::Local(query) => self.local(query, span),
            Reference::Shared(query) => self.shared(query, span),
            Reference::Thread(query) => self.threads(query, span),
            Reference::Skill(query) => self.skills(query, span),
        }
    }

    fn local(&mut self, query: &str, span: Span) -> Vec<Suggestion> {
        if outside_here(query) {
            return directory(&self.matcher, query, span);
        }
        let modes = self.modes(query, span);
        let paths = self.local.get(|| {
            mention::walk(Path::new("."))
                .take(mention::MAX_INDEXED)
                .collect()
        });
        let paths = ranked(&self.matcher, paths, query)
            .into_iter()
            .map(|(path, indices)| path_suggestion(mention::typed(path), path, indices, span, ""));
        modes.into_iter().chain(paths).take(MAX_SUGGESTED).collect()
    }

    /// `@thread:`, `@shared:` and `@skill:`, when the host has some and
    /// `query` could be typing them.
    fn modes(&self, query: &str, span: Span) -> Vec<Suggestion> {
        let index = lock(&self.index);
        [
            (THREAD, !index.threads.is_empty(), "a saved conversation"),
            (SHARED, !index.shared.is_empty(), "the host's shared files"),
            (
                SKILL,
                !index.skills.skills.is_empty(),
                "a skill's instructions",
            ),
        ]
        .into_iter()
        .filter(|(mode, offered, _)| *offered && mode.starts_with(query))
        .map(|(mode, _, description)| Suggestion {
            value: format!("@{mode}"),
            description: Some(description.into()),
            span,
            ..Suggestion::default()
        })
        .collect()
    }

    fn shared(&self, query: &str, span: Span) -> Vec<Suggestion> {
        let index = lock(&self.index);
        ranked(&self.matcher, &index.shared, query)
            .into_iter()
            .take(MAX_SUGGESTED)
            .map(|(path, indices)| {
                let value = mention::typed(&format!("{SHARED}{path}"));
                path_suggestion(value, path, indices, span, "shared ")
            })
            .collect()
    }

    fn threads(&self, query: &str, span: Span) -> Vec<Suggestion> {
        let index = lock(&self.index);
        thread_matches(&self.matcher, &index.threads, query)
            .take(MAX_SUGGESTED)
            .map(|summary| Suggestion {
                value: format!("@{THREAD}{}", summary.id.as_str()),
                ..thread_suggestion(summary, span)
            })
            .collect()
    }

    fn skills(&self, query: &str, span: Span) -> Vec<Suggestion> {
        let index = lock(&self.index);
        skill_suggestions(&self.matcher, &index.skills.skills, query)
            .map(|(skill, indices)| Suggestion {
                value: format!("@{SKILL}{}", skill.name),
                ..skill_suggestion(skill, indices, span)
            })
            .collect()
    }
}

/// Whether `query` names a path by where it is, rather than by what is under
/// the working directory: home, absolute, or relative to a parent.
fn outside_here(query: &str) -> bool {
    matches!(query, "~" | "." | "..")
        || ["~/", "/", "./", "../"]
            .iter()
            .any(|prefix| query.starts_with(prefix))
}

/// The entries of the directory `query` names up to its last `/`, whose
/// names match what follows it; directories end in `/` and are not followed
/// by a space, so that completion goes on into them.
fn directory(matcher: &SkimMatcherV2, query: &str, span: Span) -> Vec<Suggestion> {
    let Some(slash) = query.rfind('/') else {
        // `~`, `.` and `..` alone: the directory itself.
        return vec![path_suggestion(
            mention::typed(&format!("{query}/")),
            &format!("{query}/"),
            Vec::new(),
            span,
            "",
        )];
    };
    let (dir, name) = (&query[..=slash], &query[slash + 1..]);
    let Ok(entries) = std::fs::read_dir(shellexpand::tilde(dir).as_ref()) else {
        return Vec::new();
    };
    let mut found: Vec<(i64, String, bool, Vec<usize>)> = entries
        .filter_map(Result::ok)
        .take(5000)
        .filter_map(|entry| {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file.starts_with('.') && !name.starts_with('.') {
                return None;
            }
            let is_dir = std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir());
            let (score, indices) = match name.is_empty() {
                true => (0, Vec::new()),
                false => matcher.fuzzy_indices(&file, name)?,
            };
            Some((score, file, is_dir, indices))
        })
        .collect();
    found.sort_by(|a, b| (Reverse(a.0), !a.2, &a.1).cmp(&(Reverse(b.0), !b.2, &b.1)));
    found
        .into_iter()
        .take(MAX_SUGGESTED)
        .map(|(_, file, is_dir, indices)| {
            let shown = match is_dir {
                true => format!("{file}/"),
                false => file,
            };
            Suggestion {
                value: mention::typed(&format!("{dir}{shown}")),
                display_override: Some(shown),
                description: Some(if is_dir { "directory" } else { "file" }.into()),
                span,
                append_whitespace: !is_dir,
                match_indices: Some(indices),
                ..Suggestion::default()
            }
        })
        .collect()
}

/// The `threads` whose date and title match `query`, best first; among
/// equals, in the order given.
pub fn thread_matches<'a>(
    matcher: &SkimMatcherV2,
    threads: impl IntoIterator<Item = &'a Summary>,
    query: &str,
) -> impl Iterator<Item = &'a Summary> {
    let mut matches: Vec<_> = threads
        .into_iter()
        .filter_map(|summary| Some((matcher.fuzzy_match(&summary.to_string(), query)?, summary)))
        .collect();
    matches.sort_by_key(|(score, _)| Reverse(*score));
    matches.into_iter().map(|(_, summary)| summary)
}

/// `summary` in a menu, shown by date with its title; its value is its id.
pub fn thread_suggestion(summary: &Summary, span: Span) -> Suggestion {
    Suggestion {
        value: summary.id.as_str().to_owned(),
        display_override: Some(summary.id.to_string()),
        description: Some(summary.title.clone()),
        span,
        append_whitespace: true,
        ..Suggestion::default()
    }
}

/// The `agents` whose id or label match `query`, best first.
pub fn agent_matches<'a>(
    matcher: &SkimMatcherV2,
    agents: impl IntoIterator<Item = &'a AgentInfo>,
    query: &str,
) -> impl Iterator<Item = &'a AgentInfo> {
    let mut matches: Vec<_> = agents
        .into_iter()
        .filter_map(|info| {
            let named = format!("{} {}", info.agent, info.label);
            Some((matcher.fuzzy_match(&named, query)?, info))
        })
        .collect();
    matches.sort_by_key(|(score, _)| Reverse(*score));
    matches.into_iter().map(|(_, info)| info)
}

/// A subagent in a menu, shown with its label; its value is its id.
pub fn agent_suggestion(info: &AgentInfo, span: Span) -> Suggestion {
    let working = if info.working { " (working)" } else { "" };
    Suggestion {
        value: info.agent.as_str().to_owned(),
        description: Some(format!("{}{working}", info.label)),
        span,
        ..Suggestion::default()
    }
}

/// The `skills` whose names match `query`, best first, each with the
/// indices of its characters that matched.
pub fn skill_suggestions<'a>(
    matcher: &SkimMatcherV2,
    skills: impl IntoIterator<Item = &'a Skill>,
    query: &str,
) -> impl Iterator<Item = (&'a Skill, Vec<usize>)> {
    let mut matches: Vec<_> = skills
        .into_iter()
        .filter_map(|skill| {
            let (score, indices) = matcher.fuzzy_indices(skill.name.as_str(), query)?;
            Some((score, skill, indices))
        })
        .collect();
    matches.sort_by_key(|(score, skill, _)| (Reverse(*score), &skill.name));
    matches
        .into_iter()
        .take(MAX_SUGGESTED)
        .map(|(_, skill, indices)| (skill, indices))
}

/// `skill` in a menu, shown by name with its description; its value is
/// left to the caller.
pub fn skill_suggestion(skill: &Skill, indices: Vec<usize>, span: Span) -> Suggestion {
    Suggestion {
        value: skill.name.to_string(),
        display_override: Some(skill.name.to_string()),
        description: Some(skill.description.clone()),
        span,
        append_whitespace: true,
        match_indices: Some(indices),
        ..Suggestion::default()
    }
}

/// The `paths` matching `query`, best first, then shallowest, then by name;
/// each with the indices of its characters that matched.
fn ranked<'a>(
    matcher: &SkimMatcherV2,
    paths: &'a [String],
    query: &str,
) -> Vec<(&'a String, Vec<usize>)> {
    let mut matches: Vec<_> = paths
        .iter()
        .filter_map(|path| {
            let (score, indices) = matcher.fuzzy_indices(path, query)?;
            Some((score, path, indices))
        })
        .collect();
    matches.sort_by_key(|(score, path, _)| (Reverse(*score), depth(path), *path));
    matches
        .into_iter()
        .map(|(_, path, indices)| (path, indices))
        .collect()
}

fn path_suggestion(
    value: String,
    path: &str,
    indices: Vec<usize>,
    span: Span,
    origin: &str,
) -> Suggestion {
    let kind = match path.ends_with('/') {
        true => "directory",
        false => "file",
    };
    Suggestion {
        value,
        display_override: Some(path.to_owned()),
        description: Some(format!("{origin}{kind}")),
        span,
        append_whitespace: true,
        match_indices: Some(indices),
        ..Suggestion::default()
    }
}

fn depth(path: &str) -> usize {
    path.trim_end_matches('/').matches('/').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_paths_outside_the_working_directory() {
        let dir = std::env::temp_dir().join(format!("hey-complete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("notes.md"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let matcher = SkimMatcherV2::default();
        let base = format!("{}/", dir.display());
        let values = |query: &str| -> Vec<String> {
            directory(&matcher, query, Span::new(0, 0))
                .into_iter()
                .map(|s| s.value)
                .collect()
        };
        assert!(outside_here(&base) && outside_here("~/x") && outside_here("../"));
        assert!(!outside_here("src/x"));
        assert_eq!(
            values(&base),
            [format!("@{base}sub/"), format!("@{base}notes.md")]
        );
        assert_eq!(values(&format!("{base}not")), [format!("@{base}notes.md")]);
        assert_eq!(values(&format!("{base}."))[0], format!("@{base}.hidden"));
        assert_eq!(values("~"), ["@~/"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

struct Cache<T>(Option<(Instant, T)>);

impl<T> Cache<T> {
    fn get(&mut self, load: impl FnOnce() -> T) -> &T {
        if self
            .0
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() > STALE_AFTER)
        {
            self.0 = None;
        }
        &self.0.get_or_insert_with(|| (Instant::now(), load())).1
    }
}
