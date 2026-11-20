/* -------------------------------------------------------------------------- */
/*                                checkpoint.rs                               */
/* -------------------------------------------------------------------------- */

//! Checkpoints: how the files an answer changed were before it, so `/undo`
//! can put them back. Each answer keeps the first version it saw of every
//! file it changed under the host's working directory, in the host's data
//! directory, per conversation: `write`, `edit`, `move` and `delete` keep a
//! file before they touch it, and `shell` learns what a command changed
//! from a [`snapshot`] just before and just after it, and keeps those. Calls
//! run on the user's machine through a remote are not kept.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use rig_core::completion::ToolDefinition;
use rig_core::tool::{
    ChangeKind, DynamicTool, FileChange, FilesChanged, ToolContext, ToolExecutionError,
};
use rig_tools::checkpoint::{Snapshot, Taking, snapshot};
use rig_tools::workspace::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::Undone;
use crate::sessions::SessionId;

/// The largest file kept: one larger is changed, but cannot be undone.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The most files a scan of the working directory may list before `shell`
/// calls go unchecked, so a big tree outside git does not stall each call.
/// In a git work tree, git's private index makes a snapshot cost what
/// changed, so there is no cap.
pub const MAX_SCANNED: usize = 20_000;

/// Every conversation's checkpoints.
pub struct Checkpoints {
    /// `<data>/checkpoints`: a directory per conversation, holding one per
    /// answer that changed something.
    dir: PathBuf,
    /// Where snapshots keep their scratch.
    keep: PathBuf,
    /// The directory whose files are kept.
    root: PathBuf,
}

/// How one file was before an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    was: Was,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Was {
    /// There was no file.
    Absent,
    /// Its bytes are in this file of the answer's directory.
    Kept(String),
    /// Larger than [`MAX_BYTES`], or unreadable: not kept.
    Lost(String),
}

const MANIFEST: &str = "files.json";

impl Checkpoints {
    pub fn new(data: &Path, root: PathBuf) -> Self {
        let keep = data.join("snapshots");
        if let Err(error) = rig_tools::checkpoint::forget_kept(&keep) {
            tracing::warn!("cannot remove old snapshots: {error}");
        }
        Self {
            dir: data.join("checkpoints"),
            keep,
            root,
        }
    }

    /// A new answer's checkpoint in thread `id`, saved once it keeps a file.
    pub fn turn(&self, id: &SessionId) -> Arc<Turn> {
        let thread = self.dir.join(id.as_str());
        let next = answers(&thread).last().map_or(0, |(number, _)| number + 1);
        Arc::new(Turn {
            dir: thread.join(format!("{next:06}")),
            root: self.root.clone(),
            keep: self.keep.clone(),
            kept: Mutex::default(),
            newest: Mutex::default(),
            scannable: OnceLock::new(),
        })
    }

    /// Puts the files the latest answer in thread `id` that changed any
    /// back as they were before it, and forgets its checkpoint; `None` when
    /// no answer left one.
    pub fn undo(&self, id: &SessionId) -> Result<Option<Undone>> {
        let thread = self.dir.join(id.as_str());
        let mut answers = answers(&thread);
        let Some((_, dir)) = answers.pop() else {
            return Ok(None);
        };
        let entries: Vec<Entry> = serde_json::from_slice(
            &fs::read(dir.join(MANIFEST))
                .with_context(|| format!("cannot read {}", dir.display()))?,
        )
        .with_context(|| format!("unreadable checkpoint {}", dir.display()))?;
        let mut undone = Undone {
            left: answers.len(),
            ..Undone::default()
        };
        for entry in entries {
            let shown = entry.path.display().to_string();
            let done = match &entry.was {
                Was::Absent => match fs::remove_file(&entry.path) {
                    Ok(()) => Ok(Some(&mut undone.removed)),
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.to_string()),
                },
                Was::Kept(blob) => fs::read(dir.join(blob))
                    .and_then(|bytes| {
                        if let Some(parent) = entry.path.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&entry.path, bytes)
                    })
                    .map(|()| Some(&mut undone.restored))
                    .map_err(|error| error.to_string()),
                Was::Lost(why) => Err(why.clone()),
            };
            match done {
                Ok(Some(list)) => list.push(shown),
                Ok(None) => {}
                Err(why) => undone.failed.push(format!("{shown}: {why}")),
            }
        }
        fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
        Ok(Some(undone))
    }

    /// Forgets thread `id`'s checkpoints, as when it is deleted.
    pub fn forget(&self, id: &SessionId) {
        match fs::remove_dir_all(self.dir.join(id.as_str())) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                tracing::warn!(thread = id.as_str(), "cannot remove checkpoints: {error}")
            }
            _ => {}
        }
    }
}

/// The answers of a thread that left a checkpoint, oldest first.
fn answers(thread: &Path) -> Vec<(u64, PathBuf)> {
    let mut found: Vec<(u64, PathBuf)> = fs::read_dir(thread)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join(MANIFEST).exists())
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
        .collect();
    found.sort();
    found
}

/// One answer's checkpoint.
pub struct Turn {
    dir: PathBuf,
    root: PathBuf,
    keep: PathBuf,
    kept: Mutex<Kept>,
    /// The newest snapshot a `shell` call took, which the next hands
    /// [`snapshot`] so a scan hashes again only what changed.
    newest: Mutex<Option<Snapshot>>,
    /// Whether snapshots are cheap enough to take around `shell` calls.
    scannable: OnceLock<bool>,
}

#[derive(Default)]
struct Kept {
    entries: Vec<Entry>,
    paths: HashSet<PathBuf>,
}

impl Turn {
    /// Keeps `path` as it is now, unless this answer kept it already, it is
    /// outside the working directory, or git ignores it.
    pub fn keep(&self, path: &Path) {
        self.keep_with(path, || match fs::metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
            Ok(meta) if meta.is_dir() => Err("a directory".into()),
            Ok(meta) if meta.len() > MAX_BYTES => Err("too large to keep".into()),
            Ok(_) => fs::read(path).map(Some).map_err(|error| error.to_string()),
        });
    }

    /// Keeps `path` as `read` says it was, under the same conditions.
    fn keep_with(&self, path: &Path, read: impl FnOnce() -> Result<Option<Vec<u8>>, String>) {
        if !path.starts_with(&self.root) || ignored(&self.root, path) {
            return;
        }
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.paths.contains(path) {
            return;
        }
        let was = match read() {
            Ok(None) => Was::Absent,
            Ok(Some(bytes)) => {
                let blob = kept.entries.len().to_string();
                match fs::create_dir_all(&self.dir)
                    .and_then(|()| fs::write(self.dir.join(&blob), bytes))
                {
                    Ok(()) => Was::Kept(blob),
                    Err(error) => Was::Lost(format!("not kept: {error}")),
                }
            }
            Err(why) => Was::Lost(why),
        };
        kept.paths.insert(path.to_path_buf());
        kept.entries.push(Entry {
            path: path.to_path_buf(),
            was,
        });
        let saved = fs::create_dir_all(&self.dir).and_then(|()| {
            let json = serde_json::to_vec(&kept.entries).map_err(std::io::Error::other)?;
            fs::write(self.dir.join(MANIFEST), json)
        });
        if let Err(error) = saved {
            tracing::warn!(dir = %self.dir.display(), "cannot save a checkpoint: {error}");
        }
    }

    /// Whether a snapshot of the working directory is cheap enough: it is
    /// in a git work tree, or lists at most [`MAX_SCANNED`] files.
    fn scannable(&self) -> bool {
        *self.scannable.get_or_init(|| {
            let git = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["rev-parse", "--is-inside-work-tree"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success());
            git || ignore::WalkBuilder::new(&self.root)
                .build()
                .take(MAX_SCANNED + 1)
                .count()
                <= MAX_SCANNED
        })
    }

    /// A snapshot of the working directory now, off this thread.
    async fn snapshot(self: &Arc<Self>) -> Option<Snapshot> {
        let turn = self.clone();
        let taken = rig_tools::blocking(move || {
            if !turn.scannable() {
                return Ok(None);
            }
            let earlier = turn
                .newest
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let taking = Taking {
                keep: Some(turn.keep.clone()),
                ..Taking::default()
            };
            let now = snapshot(&Local, &turn.root, &taking, earlier.as_ref())
                .map_err(|error| ToolExecutionError::other(error.to_string()))?;
            *turn.newest.lock().unwrap_or_else(PoisonError::into_inner) = Some(now.clone());
            Ok(Some(now))
        })
        .await;
        taken
            .inspect_err(|error| tracing::warn!("no snapshot around a shell call: {error}"))
            .ok()
            .flatten()
    }

    /// Keeps each file `changes` changed as `before` held it.
    fn keep_changes(&self, before: &Snapshot, changes: &[FileChange]) {
        for change in changes {
            let path = PathBuf::from(&change.path);
            self.keep_with(&path, || match change.kind {
                ChangeKind::Created => Ok(None),
                _ => match before.content(&path) {
                    Some(Ok(bytes)) => Ok(Some(bytes)),
                    Some(Err(error)) => Err(error.to_string()),
                    None => Err("its content was not kept".into()),
                },
            });
        }
    }
}

/// Whether a `.gitignore` between `root` and `path` ignores it.
fn ignored(root: &Path, path: &Path) -> bool {
    let mut dir = path.parent();
    while let Some(here) = dir.filter(|here| here.starts_with(root)) {
        let file = here.join(".gitignore");
        if file.exists() {
            let (rules, _) = Gitignore::new(&file);
            if rules.matched_path_or_any_parents(path, false).is_ignore() {
                return true;
            }
        }
        dir = here.parent();
    }
    false
}

/// `tool` (`write`, `edit`, `move` or `delete`), keeping the files a call
/// names in `turn` before it runs; relative paths start from `base`.
pub fn files(tool: DynamicTool, turn: Arc<Turn>, base: Option<PathBuf>) -> DynamicTool {
    wrap(tool, move |tool, context, args| {
        let (turn, base) = (turn.clone(), base.clone());
        Box::pin(async move {
            for name in ["path", "from", "to"] {
                if let Some(path) = args.get(name).and_then(Value::as_str) {
                    turn.keep(&rig_tools::fs::resolve(base.as_deref(), path));
                }
            }
            tool.run_with(context, args).await
        })
    })
}

/// `tool` (`shell`), taking a snapshot just before and after each call,
/// keeping in `turn` the files it changed, and reporting them as `write`
/// and `edit` report theirs.
pub fn shell(tool: DynamicTool, turn: Arc<Turn>) -> DynamicTool {
    wrap(tool, move |tool, context, args| {
        let turn = turn.clone();
        Box::pin(async move {
            let before = turn.snapshot().await;
            let result = tool.run_with(context, args).await;
            if let Some(before) = before
                && let Some(after) = turn.snapshot().await
            {
                match after.changes_since(&before) {
                    Ok(changes) if !changes.is_empty() => {
                        turn.keep_changes(&before, &changes);
                        if context.result::<FilesChanged>().ok().flatten().is_none() {
                            context.insert_result(FilesChanged(changes)).ok();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!("no changes of a shell call: {error}"),
                }
            }
            result
        })
    })
}

type Future<'a> =
    futures::future::BoxFuture<'a, Result<rig_core::tool::ToolOutput, ToolExecutionError>>;

/// `tool` with the same definition, its calls going through `call`.
fn wrap<F>(tool: DynamicTool, call: F) -> DynamicTool
where
    F: for<'a> Fn(DynamicTool, &'a mut ToolContext, Value) -> Future<'a> + Send + Sync + 'static,
{
    let ToolDefinition {
        name,
        description,
        parameters,
        annotations,
        ..
    } = tool.definition();
    DynamicTool::new_with_context(name, description, parameters, move |context, args| {
        call(tool.clone(), context, args)
    })
    .with_annotations(annotations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hey-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn undo_restores_answers_one_at_a_time_newest_first() {
        let base = scratch("undo");
        let (root, data) = (base.join("work"), base.join("data"));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        let checkpoints = Checkpoints::new(&data, root.clone());
        let id = SessionId::new();

        let first = checkpoints.turn(&id);
        first.keep(&root.join("a.txt"));
        fs::write(root.join("a.txt"), "two").unwrap();
        drop(first);

        let second = checkpoints.turn(&id);
        second.keep(&root.join("a.txt"));
        second.keep(&root.join("new.txt"));
        second.keep(&root.join("build/out"));
        second.keep(&base.join("outside.txt"));
        fs::write(root.join("a.txt"), "three").unwrap();
        second.keep(&root.join("a.txt"));
        fs::write(root.join("new.txt"), "new").unwrap();

        let undone = checkpoints.undo(&id).unwrap().unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "two");
        assert!(!root.join("new.txt").exists());
        assert_eq!(undone.restored.len(), 1, "{undone:?}");
        assert_eq!(
            undone.removed.len(),
            1,
            "ignored and outside files are not kept"
        );
        assert_eq!(undone.left, 1);

        checkpoints.undo(&id).unwrap().unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "one");
        assert!(checkpoints.undo(&id).unwrap().is_none());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_answer_that_changed_nothing_leaves_no_checkpoint() {
        let base = scratch("undo-empty");
        let checkpoints = Checkpoints::new(&base.join("data"), base.clone());
        let id = SessionId::new();
        drop(checkpoints.turn(&id));
        assert!(checkpoints.undo(&id).unwrap().is_none());
        fs::remove_dir_all(&base).ok();
    }

    #[tokio::test]
    async fn a_shell_call_keeps_and_reports_what_it_changed() {
        let base = scratch("undo-shell");
        let (root, data) = (base.join("work"), base.join("data"));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "old").unwrap();
        let checkpoints = Checkpoints::new(&data, root.clone());
        let id = SessionId::new();
        let turn = checkpoints.turn(&id);
        let inner = crate::toolbox::dynamic(rig_tools::Shell {
            launcher: rig_tools::process::Launcher::default().in_dir(&root),
            ..rig_tools::Shell::default()
        });
        let tool = shell(inner, turn.clone());
        let mut context = ToolContext::new();
        let args = serde_json::json!({ "command": "printf new > a.txt; printf b > b.txt" });
        tool.run_with(&mut context, args).await.unwrap();
        let changed = context
            .result::<FilesChanged>()
            .unwrap()
            .unwrap_or_default();
        assert_eq!(changed.0.len(), 2, "{changed:?}");
        drop(turn);

        let undone = checkpoints.undo(&id).unwrap().unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "old");
        assert!(!root.join("b.txt").exists());
        assert_eq!((undone.restored.len(), undone.removed.len()), (1, 1));
        fs::remove_dir_all(&base).ok();
    }
}
