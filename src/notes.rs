/* -------------------------------------------------------------------------- */
/*                                  notes.rs                                  */
/* -------------------------------------------------------------------------- */

//! What a conversation keeps on the host besides its messages: whether it
//! is in plan mode, and the agent's todo list. Saved beside the
//! conversations, so both outlast the host.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;

/// One item of the agent's todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Todo {
    /// What to do, in a few words.
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    /// Also spelled as other agents' todo tools do.
    #[serde(alias = "completed", alias = "complete", alias = "cancelled")]
    Done,
}

impl TodoStatus {
    /// How a list shows it.
    pub fn mark(self) -> &'static str {
        match self {
            TodoStatus::Pending => "☐",
            TodoStatus::InProgress => "◐",
            TodoStatus::Done => "✔",
        }
    }
}

/// `todos` as a summary keeps them, in Markdown.
pub fn todo_section(todos: &[Todo]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    let items: Vec<String> = todos
        .iter()
        .map(|todo| {
            let mark = match todo.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Done => "[x]",
            };
            format!("- {mark} {}", todo.text)
        })
        .collect();
    Some(format!("## Todo list\n{}", items.join("\n")))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    plan: bool,
    #[serde(default)]
    todos: Vec<Todo>,
}

/// A conversation's notes. Plan mode lives in its tool permissions, which
/// its subagents share, so their calls are held to it too.
pub struct Notes {
    file: Option<PathBuf>,
    permissions: Arc<Permissions>,
    todos: Mutex<Vec<Todo>>,
}

impl Notes {
    /// The notes saved in `file`, if any, applying plan mode to
    /// `permissions`; without a file, kept only in memory.
    pub fn load(file: Option<PathBuf>, permissions: Arc<Permissions>) -> Self {
        let saved: Saved = file
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        permissions.set_planning(saved.plan);
        Self {
            file,
            permissions,
            todos: Mutex::new(saved.todos),
        }
    }

    pub fn planning(&self) -> bool {
        self.permissions.planning()
    }

    pub fn set_planning(&self, on: bool) {
        self.permissions.set_planning(on);
        self.save();
    }

    pub fn todos(&self) -> Vec<Todo> {
        self.lock().clone()
    }

    pub fn set_todos(&self, todos: Vec<Todo>) {
        *self.lock() = todos;
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let saved = Saved {
            plan: self.planning(),
            todos: self.todos(),
        };
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(file, serde_json::to_vec(&saved).unwrap_or_default()));
        if let Err(error) = written {
            tracing::warn!(file = %file.display(), %error, "cannot save a conversation's notes");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Todo>> {
        self.todos.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_outlast_the_host() {
        let file = std::env::temp_dir().join(format!("hey-notes-test-{}.json", std::process::id()));
        let todos = vec![Todo {
            text: "write tests".into(),
            status: TodoStatus::InProgress,
        }];
        let notes = Notes::load(Some(file.clone()), Arc::default());
        notes.set_planning(true);
        notes.set_todos(todos.clone());

        let permissions = Arc::new(Permissions::default());
        let again = Notes::load(Some(file.clone()), permissions.clone());
        assert!(permissions.planning(), "plan mode reaches the permissions");
        assert_eq!(again.todos(), todos);
        assert_eq!(
            todo_section(&todos).as_deref(),
            Some("## Todo list\n- [~] write tests")
        );
        _ = std::fs::remove_file(file);
    }
}
