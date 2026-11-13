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

use crate::mention::{self, Item, Reference, SHARED, SKILL, THREAD};
use crate::sessions::Summary;
use crate::skills::{Found, Skill};

/// How long a walk of the working directory, or what the host offers,
/// answers completions before it is fetched again.
pub const STALE_AFTER: Duration = Duration::from_secs(5);
const MAX_SUGGESTED: usize = 50;

/// What the host offers to mention, as last fetched.
#[derive(Debug, Default)]
pub struct HostIndex {
    pub threads: Vec<Summary>,
    pub shared: Vec<String>,
    pub skills: Found,
}

pub type Index = Arc<Mutex<HostIndex>>;

pub fn lock(index: &Index) -> MutexGuard<'_, HostIndex> {
    index.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HostIndex {
    /// The skill `/name` invokes: none a command takes the place of.
    pub fn command(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name).filter(|_| commands::find(name).is_none())
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
            Reference::Skill(name) => self.skills.get(name).map(|skill| format!("{SKILL}{}", skill.name)),
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

    pub fn suggestions(&mut self, before: &str) -> Vec<Suggestion> {
        let Some(typed) = mention::under(before, before.len()) else {
            return Vec::new();
        };
        let span = Span::new(typed.span.start, typed.span.end);
        match typed.reference() {
            Reference::Local(query) => self.local(query, span),
            Reference::Shared(query) => self.shared(query, span),
            Reference::Thread(query) => self.threads(query, span),
            Reference::Skill(query) => self.skills(query, span),
        }
    }

    fn local(&mut self, query: &str, span: Span) -> Vec<Suggestion> {
        let modes = self.modes(query, span);
        let paths = self
            .local
            .get(|| mention::walk(Path::new(".")).take(mention::MAX_INDEXED).collect());
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
            (SKILL, !index.skills.skills.is_empty(), "a skill's instructions"),
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
        let mut matches: Vec<_> = index
            .threads
            .iter()
            .filter_map(|summary| {
                let score = self.matcher.fuzzy_match(&summary.to_string(), query)?;
                Some((score, summary))
            })
            .collect();
        matches.sort_by_key(|(score, _)| Reverse(*score));
        matches
            .into_iter()
            .take(MAX_SUGGESTED)
            .map(|(_, summary)| Suggestion {
                value: format!("@{THREAD}{}", summary.id.as_str()),
                display_override: Some(summary.id.to_string()),
                description: Some(summary.title.clone()),
                span,
                append_whitespace: true,
                ..Suggestion::default()
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

struct Cache<T>(Option<(Instant, T)>);

impl<T> Cache<T> {
    fn get(&mut self, load: impl FnOnce() -> T) -> &T {
        if self.0.as_ref().is_some_and(|(at, _)| at.elapsed() > STALE_AFTER) {
            self.0 = None;
        }
        &self.0.get_or_insert_with(|| (Instant::now(), load())).1
    }
}
