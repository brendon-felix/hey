/* -------------------------------------------------------------------------- */
/*                               mention/mod.rs                               */
/* -------------------------------------------------------------------------- */

//! `@` mentions in a message. The client attaches those of its user's
//! working directory; the host, those of its shared repository, its saved
//! conversations and its skills.

mod context;

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use ignore::WalkBuilder;

use crate::protocol::Attachment;

pub use context::{Context, is_context, transcript};

/// What precedes a saved conversation's id in a mention, as Zed's `@thread`.
pub const THREAD: &str = "thread:";

/// What precedes a path in the host's shared repository.
pub const SHARED: &str = "shared:";

/// What precedes a skill's name, as Zed's `@skill`.
pub const SKILL: &str = "skill:";

/// The most paths a walk offers for completion.
pub const MAX_INDEXED: usize = 10_000;

const MAX_LISTED: usize = 500;

/// Ends a sentence rather than a path, when what precedes it names something.
const PUNCTUATION: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'];

/// What a mention's text refers to, and so where it is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reference<'a> {
    /// A path in the user's working directory, which the client attaches.
    Local(&'a str),
    /// A path in the host's shared repository.
    Shared(&'a str),
    /// A saved conversation's id, on the host.
    Thread(&'a str),
    /// A skill's name, on the host.
    Skill(&'a str),
}

impl<'a> Reference<'a> {
    pub fn of(text: &'a str) -> Self {
        if let Some(id) = text.strip_prefix(THREAD) {
            Self::Thread(id)
        } else if let Some(path) = text.strip_prefix(SHARED) {
            Self::Shared(path)
        } else if let Some(name) = text.strip_prefix(SKILL) {
            Self::Skill(name)
        } else {
            Self::Local(text)
        }
    }
}

/// An `@` and the text after it, as typed: a run of non-whitespace, or a
/// `"`-quoted one that may hold spaces.
#[derive(Debug, PartialEq, Eq)]
pub struct Mention<'a> {
    pub span: Range<usize>,
    pub text: &'a str,
    quoted: bool,
}

impl<'a> Mention<'a> {
    pub fn reference(&self) -> Reference<'a> {
        Reference::of(self.text)
    }

    /// What `name` finds this refers to, and the span referring to it:
    /// punctuation ending an unquoted mention is left out unless the whole of
    /// it names something.
    pub fn resolve<T>(&self, name: impl Fn(Reference<'a>) -> Option<T>) -> Option<(T, Range<usize>)> {
        let trimmed = match self.quoted {
            true => self.text,
            false => self.text.trim_end_matches(PUNCTUATION),
        };
        std::iter::once(self.text)
            .chain((trimmed != self.text).then_some(trimmed))
            .find_map(|text| {
                let found = name(Reference::of(text))?;
                let cut = self.text.len() - text.len();
                Some((found, self.span.start..self.span.end - cut))
            })
    }
}

/// The mentions in `text`, in order. Like Zed, an `@` starts one only at the
/// start of the text or after whitespace or an opening bracket, so an email
/// address is not one.
pub fn mentions(text: &str) -> impl Iterator<Item = Mention<'_>> {
    let mut from = 0;
    std::iter::from_fn(move || {
        while let Some(offset) = text[from..].find('@') {
            let at = from + offset;
            from = at + 1;
            if let Some(mention) = lex(text, at) {
                from = mention.span.end;
                return Some(mention);
            }
        }
        None
    })
}

/// The mention that `cursor` is in or just after.
pub fn under(text: &str, cursor: usize) -> Option<Mention<'_>> {
    mentions(text).find(|mention| mention.span.start < cursor && cursor <= mention.span.end)
}

/// How a mention of `text` is typed.
pub fn typed(text: &str) -> String {
    match text.contains(char::is_whitespace) {
        true => format!("@\"{text}\""),
        false => format!("@{text}"),
    }
}

fn lex(text: &str, at: usize) -> Option<Mention<'_>> {
    let starts = text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{'));
    if !starts {
        return None;
    }
    let rest = &text[at + 1..];
    let (text, len, quoted) = match rest.strip_prefix('"') {
        Some(inner) => match inner.find('"') {
            Some(end) => (&inner[..end], end + 2, true),
            None => (inner, rest.len(), true),
        },
        None => {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], end, false)
        }
    };
    Some(Mention {
        span: at..at + 1 + len,
        text,
        quoted,
    })
}

/// The files and directories `prompt` mentions in the user's working
/// directory, each once.
pub fn local(prompt: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let found = mentions(prompt).filter_map(|mention| {
        mention.resolve(|reference| match reference {
            Reference::Local(path) => Item::local(path),
            Reference::Shared(_) | Reference::Thread(_) | Reference::Skill(_) => None,
        })
    });
    for (item, _) in found {
        if !items.contains(&item) {
            items.push(item);
        }
    }
    items
}

/// A file or directory a mention names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    File(PathBuf),
    Directory(PathBuf),
}

impl Item {
    pub fn at(path: PathBuf) -> Option<Self> {
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.is_dir() {
            Some(Self::Directory(path))
        } else if metadata.is_file() {
            Some(Self::File(path))
        } else {
            None
        }
    }

    /// What `path`, as typed in the user's working directory, names.
    pub fn local(path: &str) -> Option<Self> {
        match path {
            "" => None,
            path => Self::at(PathBuf::from(shellexpand::tilde(path).as_ref())),
        }
    }

    /// The file whole, if the read tool would show it whole, or the
    /// directory's listing.
    pub fn load(&self) -> Result<Attachment> {
        match self {
            Self::File(path) => {
                let size = std::fs::metadata(path)?.len();
                if size > rig_tools::MAX_BYTES as u64 {
                    bail!(
                        "larger than {} KiB; the model can read it in parts",
                        rig_tools::MAX_BYTES / 1024
                    );
                }
                let text =
                    String::from_utf8(std::fs::read(path)?).map_err(|_| anyhow!("not text"))?;
                Ok(Attachment::File {
                    path: absolute(path)?,
                    text,
                })
            }
            Self::Directory(path) => {
                let mut walk = walk(path);
                let entries = walk.by_ref().take(MAX_LISTED).collect();
                Ok(Attachment::Directory {
                    path: absolute(path)?,
                    entries,
                    truncated: walk.next().is_some(),
                })
            }
        }
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Directory(path) if path.to_string_lossy().ends_with('/') => {
                write!(f, "{}", path.display())
            }
            Self::Directory(path) => write!(f, "{}/", path.display()),
        }
    }
}

fn absolute(path: &Path) -> Result<String> {
    Ok(std::path::absolute(path)?.display().to_string())
}

/// Paths under `root`, relative to it, that git would not ignore, in name
/// order, each directory's ending in `/`.
pub fn walk(root: &Path) -> impl Iterator<Item = String> + use<> {
    let base = root.to_path_buf();
    WalkBuilder::new(root)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0)
        .map(move |entry| {
            let path = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            let slash = match entry.file_type().is_some_and(|kind| kind.is_dir()) {
                true => "/",
                false => "",
            };
            format!("{}{slash}", path.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str) -> Vec<&str> {
        mentions(text).map(|mention| mention.text).collect()
    }

    #[test]
    fn finds_mentions_where_they_can_start() {
        assert_eq!(texts("@a (@b) x@c [@d e"), ["a", "b)", "d"]);
        assert_eq!(
            texts(r#"see @"my notes.md" and @"open @x"#),
            ["my notes.md", "open @x"]
        );
        assert_eq!(texts("@ trailing @"), ["", ""]);
        let typed = under("look @src/ma", 12).map(|mention| mention.span);
        assert_eq!(typed, Some(5..12));
        assert_eq!(under("@src now", 5), None);
    }

    #[test]
    fn sorts_local_shared_and_thread_mentions_and_drops_sentence_punctuation() {
        let text = "read @Cargo.toml, @shared:guide.md and @thread:x. Not @nope; @skill:pdf.";
        let resolved: Vec<_> = mentions(text)
            .filter_map(|mention| {
                mention.resolve(|reference| match reference {
                    Reference::Local(path) => Item::local(path).map(|_| reference),
                    Reference::Shared("guide.md") | Reference::Thread("x") | Reference::Skill("pdf") => {
                        Some(reference)
                    }
                    Reference::Shared(_) | Reference::Thread(_) | Reference::Skill(_) => None,
                })
            })
            .map(|(reference, span)| (reference, &text[span]))
            .collect();
        assert_eq!(
            resolved,
            [
                (Reference::Local("Cargo.toml"), "@Cargo.toml"),
                (Reference::Shared("guide.md"), "@shared:guide.md"),
                (Reference::Thread("x"), "@thread:x"),
                (Reference::Skill("pdf"), "@skill:pdf"),
            ]
        );
    }
}
