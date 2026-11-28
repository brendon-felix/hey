/* -------------------------------------------------------------------------- */
/*                               acp/commands.rs                              */
/* -------------------------------------------------------------------------- */

//! The slash commands an editor offers for a hey session: the REPL's, as far
//! as an editor can use them, and the skills and MCP prompts a message
//! invokes as `/name`. Commands for the terminal itself (`/theme`, `/copy`,
//! `/threads`, `/clear`, `/exit`...) have no place here.

use agent_client_protocol::v1::schema::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};
use anyhow::Result;

use super::{Session, Shared};
use crate::config::Thinking;
use crate::permissions::Permission;
use crate::sessions::SessionId;

/// What typing a message did.
pub enum Outcome {
    /// A command ran; this is what it says.
    Said(String),
    /// Not a command: this goes to the model, as a skill or MCP prompt
    /// invocation if it starts with one.
    Ask(String),
    /// A command that has the model work.
    Run(Run),
}

pub enum Run {
    Compact(String),
    Answer(String),
}

/// A command the editor lists: its name, what it does, and a hint for its
/// argument, if it takes one.
struct Spec(&'static str, &'static str, Option<&'static str>);

const COMMANDS: &[Spec] = &[
    Spec("help", "List the commands", None),
    Spec(
        "add-dir",
        "Add a folder to this conversation's workspace",
        Some("path of the folder"),
    ),
    Spec("new", "Start a new conversation in this thread", None),
    Spec("fork", "Continue in a copy of this conversation", None),
    Spec(
        "compact",
        "Replace older messages with a summary",
        Some("what the summary should keep"),
    ),
    Spec("undo", "Put back the files the latest answer changed", None),
    Spec(
        "plan",
        "Toggle plan mode: only tools that change nothing, until you approve a plan",
        None,
    ),
    Spec(
        "goal",
        "Keep working until a condition is met; without one, clear it",
        Some("condition"),
    ),
    Spec(
        "model",
        "Switch model, or list the models",
        Some("vendor/model"),
    ),
    Spec(
        "think",
        "Set thinking: off, minimal, low, medium, high, xhigh, max or default",
        Some("level"),
    ),
    Spec(
        "tools",
        "List tools, their permissions and MCP servers",
        None,
    ),
    Spec("skills", "List skills, which /name invokes", None),
    Spec(
        "agents",
        "List the kinds of subagent the agent may start",
        None,
    ),
    Spec("memories", "List what hey remembers", None),
    Spec(
        "init",
        "Have the model write or update the project's AGENTS.md",
        None,
    ),
];

/// Every command the editor can offer this session: those above, then the
/// skills and MCP prompts, unless a command has their name.
pub async fn available(shared: &Shared) -> Vec<AvailableCommand> {
    let command = |name: &str, about: &str, hint: Option<&str>| {
        let command = AvailableCommand::new(name, about);
        match hint {
            Some(hint) => command.input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new(hint),
            )),
            None => command,
        }
    };
    let mut commands: Vec<AvailableCommand> = COMMANDS
        .iter()
        .map(|Spec(name, about, hint)| command(name, about, *hint))
        .collect();
    if let Ok(found) = shared.backend.skills().await {
        for skill in found.skills {
            if builtin(skill.name.as_str()).is_none() {
                commands.push(command(
                    skill.name.as_str(),
                    &skill.description,
                    Some("what to use it for"),
                ));
            }
        }
    }
    if let Ok(prompts) = shared.backend.prompts().await {
        for prompt in prompts {
            let about = prompt
                .description
                .clone()
                .unwrap_or_else(|| format!("{}'s prompt {}", prompt.server, prompt.name));
            let hint = (!prompt.arguments.is_empty()).then(|| prompt.arguments.join(" "));
            commands.push(command(&prompt.command(), &about, hint.as_deref()));
        }
    }
    commands
}

fn builtin(word: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|Spec(name, ..)| *name == word)
}

/// Runs `text`, typed in session `key`, if it is a command.
pub async fn run(shared: &Shared, key: &str, text: &str) -> Result<Outcome> {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Ok(Outcome::Ask(text.to_owned()));
    };
    let (word, argument) = match rest.split_once(char::is_whitespace) {
        Some((word, argument)) => (word, argument.trim()),
        None => (rest, ""),
    };
    // `/usr/bin` is a path, not a command.
    if word.is_empty() || word.contains('/') {
        return Ok(Outcome::Ask(text.to_owned()));
    }
    let said = |text: String| Ok(Outcome::Said(text));
    match word {
        "help" => said(help(shared).await),
        "add-dir" => said(add_dir(shared, key, argument)),
        "new" => {
            with(shared, key, |session| {
                session.thread = SessionId::new();
                session.told = false;
                session.plan = false;
            })?;
            said("Started a new conversation.".into())
        }
        "fork" => {
            let thread = thread(shared, key)?;
            let copy = shared.backend.fork(&thread).await?;
            with(shared, key, |session| session.thread = copy)?;
            said("Continuing in a copy of this conversation.".into())
        }
        "compact" => Ok(Outcome::Run(Run::Compact(argument.to_owned()))),
        "undo" => said(undo(shared, key).await?),
        "plan" => {
            let (thread, on) = with(shared, key, |session| {
                session.plan = !session.plan;
                (session.thread.clone(), session.plan)
            })?;
            shared.backend.set_plan(&thread, on).await?;
            said(
                match on {
                    true => {
                        "Plan mode: only tools that change nothing run, until you approve a plan."
                    }
                    false => "Left plan mode.",
                }
                .into(),
            )
        }
        "goal" => {
            let goal = (!argument.is_empty()).then(|| argument.to_owned());
            let told = match &goal {
                Some(goal) => format!("Goal: {goal}"),
                None => "Goal cleared.".into(),
            };
            with(shared, key, |session| session.goal = goal)?;
            said(told)
        }
        "model" => said(model(shared, key, argument).await?),
        "think" => said(think(shared, key, argument)?),
        "tools" => said(tools(shared, key).await?),
        "skills" => said(skills(shared).await?),
        "agents" => said(agents(shared).await?),
        "memories" => said(memories(shared).await?),
        "init" => Ok(Outcome::Run(Run::Answer(
            crate::instructions::INIT_PROMPT.to_owned(),
        ))),
        word => {
            // A skill or an MCP prompt: the host expands it.
            let skill = match shared.backend.skills().await {
                Ok(found) => found.skills.iter().any(|skill| skill.name.as_str() == word),
                Err(_) => false,
            };
            let prompt = match shared.backend.prompts().await {
                Ok(prompts) => crate::mcp::invoked(&prompts, word).is_some(),
                Err(_) => false,
            };
            match skill || prompt {
                true => Ok(Outcome::Ask(text.to_owned())),
                false => said(format!(
                    "Unknown command `/{word}`. `/help` lists what this editor can run."
                )),
            }
        }
    }
}

fn with<T>(shared: &Shared, key: &str, change: impl FnOnce(&mut Session) -> T) -> Result<T> {
    let mut sessions = shared.sessions.lock().unwrap();
    let session = sessions
        .get_mut(key)
        .ok_or_else(|| anyhow::anyhow!("unknown session {key}"))?;
    Ok(change(session))
}

fn thread(shared: &Shared, key: &str) -> Result<SessionId> {
    with(shared, key, |session| session.thread.clone())
}

async fn help(shared: &Shared) -> String {
    let mut lines = vec!["Commands:".to_owned()];
    for command in available(shared).await {
        lines.push(format!("- `/{}`: {}", command.name, command.description));
    }
    lines.join("\n")
}

/// Adds the folder `argument` names to the session's workspace, and says
/// what came of it. The model is told of the new list with the next message.
fn add_dir(shared: &Shared, key: &str, argument: &str) -> String {
    if argument.is_empty() {
        return "Usage: `/add-dir <path>`".into();
    }
    let expanded = match crate::config::expand_path(argument) {
        Ok(path) => path,
        Err(error) => return format!("Cannot use `{argument}`: {error:#}"),
    };
    let added = with(shared, key, |session| {
        let path = match session.roots[0].join(&expanded).canonicalize() {
            Ok(path) if path.is_dir() => path,
            Ok(path) => return format!("`{}` is not a folder.", path.display()),
            Err(error) => return format!("Cannot use `{argument}`: {error}"),
        };
        if session.roots.contains(&path) {
            return format!("`{}` is already in the workspace.", path.display());
        }
        session.roots.push(path.clone());
        session.told = false;
        format!(
            "Added `{}`. The workspace now has {} folders.",
            path.display(),
            session.roots.len()
        )
    });
    added.unwrap_or_else(|error| format!("{error:#}"))
}

async fn undo(shared: &Shared, key: &str) -> Result<String> {
    let thread = thread(shared, key)?;
    let Some(undone) = shared.backend.undo(&thread).await? else {
        return Ok("No answer left that changed files.".into());
    };
    let mut lines = Vec::new();
    lines.extend(
        undone
            .restored
            .iter()
            .map(|path| format!("- restored `{path}`")),
    );
    lines.extend(
        undone
            .removed
            .iter()
            .map(|path| format!("- removed `{path}`")),
    );
    lines.extend(
        undone
            .failed
            .iter()
            .map(|failure| format!("- not undone: {failure}")),
    );
    lines.push(format!(
        "\nFiles only; the conversation is unchanged. {} earlier answer{} to undo.",
        undone.left,
        if undone.left == 1 { "" } else { "s" }
    ));
    Ok(lines.join("\n"))
}

async fn model(shared: &Shared, key: &str, argument: &str) -> Result<String> {
    let current = with(shared, key, |session| session.model.clone())?;
    if argument.is_empty() {
        let listing = shared.backend.models().await?;
        let mut lines = vec![format!("Using `{current}`. Models:")];
        lines.extend(listing.models.iter().map(|model| {
            let thinks = if model.thinking { " (thinks)" } else { "" };
            format!("- `{}`{thinks}", model.reference)
        }));
        lines.extend(
            listing
                .problems
                .iter()
                .map(|problem| format!("- not listed: {problem}")),
        );
        return Ok(lines.join("\n"));
    }
    let model = shared.backend.model(argument).await?;
    let told = format!("Using `{}`.", model.reference);
    with(shared, key, |session| session.model = model.reference)?;
    Ok(told)
}

fn think(shared: &Shared, key: &str, argument: &str) -> Result<String> {
    if argument.is_empty() {
        let thinking = with(shared, key, |session| session.thinking)?;
        let levels: Vec<&str> = Thinking::ALL.iter().map(|level| level.as_str()).collect();
        return Ok(format!(
            "Thinking: {}. Set one of {}, or `default`.",
            thinking.map_or("the model's default", Thinking::as_str),
            levels.join(", ")
        ));
    }
    let thinking = match argument {
        "default" => None,
        level => Some(level.parse::<Thinking>().map_err(anyhow::Error::msg)?),
    };
    with(shared, key, |session| session.thinking = thinking)?;
    Ok(format!(
        "Thinking: {}",
        thinking.map_or("the model's default", Thinking::as_str)
    ))
}

async fn tools(shared: &Shared, key: &str) -> Result<String> {
    let tools = shared.backend.tools(&thread(shared, key)?).await?;
    let mut lines = Vec::new();
    for (tool, permission) in tools.permissions {
        let permission = match permission {
            Permission::Allow => "allow",
            Permission::Confirm => "confirm",
            Permission::Deny => "deny",
        };
        lines.push(format!("- {permission}: `{tool}`"));
    }
    if !tools.servers.is_empty() {
        lines.push("\nMCP servers:".into());
        for server in tools.servers {
            if server.pending {
                lines.push(format!("- {} (connecting…)", server.name));
                continue;
            }
            lines.push(match server.tools {
                Ok(count) => format!("- {} ({count} tools)", server.name),
                Err(error) => format!("- {}: {error}", server.name),
            });
        }
    }
    Ok(lines.join("\n"))
}

async fn skills(shared: &Shared) -> Result<String> {
    let found = shared.backend.skills().await?;
    if found.skills.is_empty() && found.problems.is_empty() {
        return Ok("No skills. A skill is a folder holding a SKILL.md.".into());
    }
    let mut lines: Vec<String> = found
        .skills
        .iter()
        .map(|skill| format!("- `/{}`: {}", skill.name, skill.description))
        .collect();
    lines.extend(
        found
            .problems
            .iter()
            .map(|problem| format!("- not loaded: {problem}")),
    );
    Ok(lines.join("\n"))
}

async fn agents(shared: &Shared) -> Result<String> {
    let found = shared.backend.agents().await?;
    if found.kinds.is_empty() && found.problems.is_empty() {
        return Ok("No agents. An agent is a <name>.md file.".into());
    }
    let mut lines: Vec<String> = found
        .kinds
        .iter()
        .map(|kind| format!("- `{}`: {}", kind.name.as_str(), kind.description))
        .collect();
    lines.extend(
        found
            .problems
            .iter()
            .map(|problem| format!("- not loaded: {problem}")),
    );
    Ok(lines.join("\n"))
}

async fn memories(shared: &Shared) -> Result<String> {
    let memories = shared.backend.memories().await?;
    if memories.is_empty() {
        return Ok("hey remembers nothing yet.".into());
    }
    Ok(memories
        .iter()
        .map(|memory| format!("- {memory}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_listed_is_handled_and_none_repeats() {
        let mut names: Vec<_> = COMMANDS.iter().map(|Spec(name, ..)| *name).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
        assert!(builtin("compact").is_some() && builtin("theme").is_none());
    }
}
