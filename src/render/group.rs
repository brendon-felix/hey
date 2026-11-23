/* -------------------------------------------------------------------------- */
/*                               render/group.rs                              */
/* -------------------------------------------------------------------------- */

//! Hidden activity (reasoning and tool calls) gathered into one summary
//! line, as `Thought twice, ran a command, read 2 files`.



use super::{palette, screen};

/// What a piece of hidden activity counts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Thought,
    Command,
    Read,
    Search,
    WebSearch,
    List,
    Edit,
    Fetch,
    Memory,
    Skill,
    Project,
    ToolLookup,
    Mcp,
    Other,
}

impl Kind {
    /// What a call to the tool `name` counts as, or `None` for tools whose
    /// calls show whatever the display settings say: a table, the todo
    /// list, questions and subagents are part of the conversation.
    pub fn of_tool(name: &str) -> Option<Self> {
        let kind = match name {
            "shell" | "bash" | "spawn_process" | "process_output" | "process_list"
            | "process_kill" => Self::Command,
            "read" => Self::Read,
            "search" => Self::Search,
            "web_search" => Self::WebSearch,
            "list" => Self::List,
            "edit" | "write" | "move" | "delete" => Self::Edit,
            "fetch" => Self::Fetch,
            "recall_memory" => Self::Memory,
            "skill" => Self::Skill,
            "project" => Self::Project,
            "tool_search" => Self::ToolLookup,
            "mcp_call" | "mcp_resources" | "mcp_read_resource" => Self::Mcp,
            _ if name.starts_with("mcp__") => Self::Mcp,
            "table" | "ask_user" | "spawn_agents" | "compact" => return None,
            _ if name == crate::tools::TODO || name == crate::tools::EXIT_PLAN => return None,
            _ => Self::Other,
        };
        Some(kind)
    }

    fn phrase(self, count: usize) -> String {
        let times = |text: &str| match count {
            1 => text.to_owned(),
            2 => format!("{text} twice"),
            n => format!("{text} {n} times"),
        };
        let (verb, one, many) = match self {
            Self::WebSearch => return times("searched the web"),
            Self::Mcp => {
                return match count {
                    1 => "used an MCP tool".into(),
                    n => format!("used {n} MCP tools"),
                };
            }
            Self::Memory => return times("checked memory"),
            Self::Project => return times("checked the project"),
            Self::ToolLookup => return times("looked up tools"),
            Self::Thought => {
                return match count {
                    1 => "thought".into(),
                    2 => "thought twice".into(),
                    n => format!("thought {n} times"),
                };
            }
            Self::Command => ("ran", "command", "commands"),
            Self::Read => ("read", "file", "files"),
            Self::Search => ("ran", "search", "searches"),
            Self::List => ("listed", "directory", "directories"),
            Self::Edit => ("edited", "file", "files"),
            Self::Fetch => ("fetched", "page", "pages"),
            Self::Skill => ("loaded", "skill", "skills"),
            Self::Other => ("used", "tool", "tools"),
        };
        match count {
            1 => format!("{verb} a {one}"),
            n => format!("{verb} {n} {many}"),
        }
    }
}

/// The hidden activity since the last thing shown: each kind counted once,
/// where it first happened.
#[derive(Debug, Default)]
pub struct Group {
    items: Vec<(Kind, usize)>,
}

impl Group {
    pub fn add(&mut self, kind: Kind) {
        match self.items.iter_mut().find(|(known, _)| *known == kind) {
            Some((_, count)) => *count += 1,
            None => self.items.push((kind, 1)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn text(&self) -> String {
        let text = self
            .items
            .iter()
            .map(|&(kind, count)| kind.phrase(count))
            .collect::<Vec<_>>()
            .join(", ");
        let mut chars = text.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    }

    /// The summary as it is drawn, wrapped to the terminal's width.
    pub fn rows(&self) -> Vec<String> {
        let width = super::terminal_width().saturating_sub(1).max(1);
        let muted = palette().muted;
        super::wrapped(&self.text(), width)
            .iter()
            .map(|row| muted.paint(row).italic().to_string())
            .collect()
    }

    /// Prints the summary for good and starts over.
    pub fn close(&mut self) {
        if !self.is_empty() {
            screen::line("");
            self.rows().into_iter().for_each(screen::line);
            self.items = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(kinds: &[Kind]) -> String {
        let mut group = Group::default();
        kinds.iter().for_each(|&kind| group.add(kind));
        group.text()
    }

    #[test]
    fn kinds_merge_where_they_first_happened() {
        use Kind::*;
        assert_eq!(text(&[Thought, Command, Read]), "Thought, ran a command, read a file");
        assert_eq!(
            text(&[Command, Thought, Command, Command, Thought]),
            "Ran 3 commands, thought twice"
        );
        assert_eq!(text(&[Thought; 3]), "Thought 3 times");
        assert_eq!(text(&[WebSearch, Read, WebSearch]), "Searched the web twice, read a file");
        assert_eq!(text(&[Mcp]), "Used an MCP tool");
        assert_eq!(text(&[Search, Read, Read, Thought]), "Ran a search, read 2 files, thought");
    }
}
