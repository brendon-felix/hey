/* -------------------------------------------------------------------------- */
/*                             browser/entries.rs                             */
/* -------------------------------------------------------------------------- */

//! A conversation as entries to look through and unfold, as Zed's agent
//! panel shows a thread: what was said in full, thinking as a preview of
//! its latest lines, and tool calls and subagents as a line each. Built from
//! what the conversation saved, and kept up as its run goes on.

use std::time::Duration;

use rig_core::message::{AssistantContent, CallId, Message, ToolResult, UserContent};
use serde::Deserialize;
use yansi::Paint;

use crate::compaction;
use crate::mention;
use crate::protocol::{AgentId, Ending, Event};
use crate::render::{self, Style, palette};
use crate::steer;
use crate::subagents;

/// Rows of reasoning a folded thinking block shows: its latest.
const PREVIEW_ROWS: usize = 4;
/// Steps of a subagent an unfolded card shows: its latest.
const AGENT_STEPS: usize = 8;

#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

pub struct Entry {
    kind: Kind,
    expanded: bool,
    /// The rows below its header, as last drawn.
    body: Option<Body>,
}

struct Body {
    width: usize,
    expanded: bool,
    rows: Vec<String>,
}

enum Kind {
    User(String),
    Reply {
        text: String,
        open: bool,
    },
    Thinking {
        text: String,
        took: Option<Duration>,
        open: bool,
    },
    Tool(Tool),
    Agent(Agent),
    Compacted {
        summary: String,
        sizes: Option<(u64, u64)>,
    },
    Notice(String),
    Error(String),
}

struct Tool {
    name: String,
    /// Its arguments' JSON, as far as the model has written them.
    args: String,
    machine: Option<String>,
    call: Option<CallId>,
    /// What a command printed as it ran.
    output: String,
    state: ToolState,
}

enum ToolState {
    /// The model is writing it.
    Writing,
    Running,
    Done(String),
    Failed(String),
    /// It ended without a result, as when its answer was stopped.
    Unanswered,
}

/// A subagent's task, from the call that started it.
struct Agent {
    agent: Option<AgentId>,
    label: String,
    message: String,
    /// The `spawn_agents` call it is part of, and its place among the call's
    /// tasks, which the call's result reports in the same order.
    spawn: Option<(CallId, usize)>,
    steps: Vec<String>,
    state: AgentState,
}

enum AgentState {
    Waiting,
    Working,
    Done(String),
    Failed(String),
    Stopped,
}

#[derive(Deserialize)]
struct Spawned {
    agents: Vec<Task>,
}

#[derive(Deserialize)]
struct Task {
    label: String,
    message: String,
    #[serde(default)]
    agent: Option<AgentId>,
}

#[derive(Deserialize)]
struct Report {
    agent: AgentId,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl Transcript {
    /// What `messages` said, everything in them ended.
    pub fn saved(messages: &[Message]) -> Self {
        let mut transcript = Self::default();
        messages
            .iter()
            .for_each(|message| transcript.message(message));
        transcript.settle();
        transcript
    }

    pub fn entry(&mut self, index: usize) -> Option<&mut Entry> {
        self.entries.get_mut(index)
    }

    pub fn entries(&mut self) -> impl Iterator<Item = &mut Entry> {
        self.entries.iter_mut()
    }

    /// The subagent the entry at `index` stands for, once it has started.
    pub fn agent(&self, index: usize) -> Option<&AgentId> {
        match &self.entries.get(index)?.kind {
            Kind::Agent(agent) => agent.agent.as_ref(),
            _ => None,
        }
    }

    /// Unfolds the entries that unfolded in `before`, a transcript of the
    /// same conversation, where the two still agree.
    pub fn unfold_as(&mut self, before: &Transcript) {
        for (entry, was) in self.entries.iter_mut().zip(&before.entries) {
            if std::mem::discriminant(&entry.kind) != std::mem::discriminant(&was.kind) {
                break;
            }
            entry.expanded = was.expanded;
        }
    }

    pub fn user(&mut self, text: &str) {
        self.push(Kind::User(text.to_owned()));
    }

    fn message(&mut self, message: &Message) {
        match message {
            Message::System { .. } => {}
            Message::User { content } => content.iter().for_each(|content| match content {
                UserContent::Text(text) if !mention::is_context(&text.text) => {
                    match compaction::summary_in(&text.text) {
                        Some(summary) => self.push(Kind::Compacted {
                            summary: summary.to_owned(),
                            sizes: None,
                        }),
                        None => self.user(&text.text),
                    }
                }
                UserContent::ToolResult(result) => {
                    let (result, steered) = steer::split(result.clone());
                    self.result(&result);
                    steered.iter().for_each(|prompt| self.user(prompt));
                }
                _ => {}
            }),
            Message::Assistant(assistant) => {
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text(text) => self.text(&text.text),
                        AssistantContent::Reasoning(reasoning) => self.reasoning(&reasoning.text),
                        AssistantContent::ToolCall(call) => {
                            let args = serde_json::Value::Object(call.function.arguments.clone());
                            self.call(
                                call.function.name.as_str(),
                                &args.to_string(),
                                None,
                                Some(call.id.clone()),
                            );
                        }
                        _ => {}
                    }
                    self.close();
                }
            }
        }
    }

    /// Takes in what a run did next.
    pub fn advance(&mut self, event: &Event) {
        match event {
            Event::Text { text } => self.text(text),
            Event::Reasoning { text } => self.reasoning(text),
            Event::PartEnd => self.close(),
            Event::Thought { duration } => self.thought(*duration),
            Event::ToolCallStart { name } => self.push(Kind::Tool(Tool {
                name: name.clone(),
                args: String::new(),
                machine: None,
                call: None,
                output: String::new(),
                state: ToolState::Writing,
            })),
            Event::ToolCallArguments { json } => {
                if let Some(tool) = self
                    .tools()
                    .rev()
                    .find(|tool| matches!(tool.state, ToolState::Writing))
                {
                    tool.args.push_str(json);
                }
                self.touch_last_tool();
            }
            Event::ToolCall {
                name,
                args,
                machine,
                call,
            } => self.call(name, args, machine.clone(), call.clone()),
            Event::Output { text } => {
                if let Some(at) = self.last_tool(|tool| matches!(tool.state, ToolState::Running)) {
                    let entry = &mut self.entries[at];
                    if let Kind::Tool(tool) = &mut entry.kind {
                        tool.output.push_str(text);
                    }
                    entry.body = None;
                }
            }
            Event::ToolResult { result } => self.result(result),
            Event::Notice { text } => self.push(Kind::Notice(text.clone())),
            Event::Attached { name } => self.push(Kind::Notice(format!("@ {name}"))),
            Event::Steered { prompt } => self.user(prompt),
            Event::Plan { on: true } => self.push(Kind::Notice("plan mode".into())),
            Event::Plan { on: false } => self.push(Kind::Notice("left plan mode".into())),
            Event::Compacted {
                summary,
                before,
                after,
            } => self.push(Kind::Compacted {
                summary: summary.clone(),
                sizes: Some((*before, *after)),
            }),
            Event::AgentStarted { agent, label } => self.agent_started(agent, label),
            Event::Agent { agent, step } => {
                if let Some(step) = step_line(step) {
                    self.with_agent(agent, |card| card.steps.push(step));
                }
            }
            Event::AgentEnded { agent, error } => self.with_agent(agent, |card| {
                card.state = match error {
                    Some(error) => AgentState::Failed(error.clone()),
                    None => AgentState::Done(String::new()),
                };
            }),
            Event::End { ending, .. } => {
                self.settle();
                match ending {
                    Ending::Answered => {}
                    Ending::Interrupted => self.push(Kind::Notice("interrupted".into())),
                    Ending::Failed { error } => self.push(Kind::Error(error.clone())),
                }
            }
            Event::Began { .. }
            | Event::Approval { .. }
            | Event::Decided { .. }
            | Event::ClientCall { .. }
            | Event::Claimed { .. }
            | Event::Context { .. } => {}
        }
    }

    /// Ends what is open: text and thinking, and calls and subagents that
    /// had no result.
    pub fn settle(&mut self) {
        self.close();
        for entry in &mut self.entries {
            match &mut entry.kind {
                Kind::Tool(tool)
                    if matches!(tool.state, ToolState::Writing | ToolState::Running) =>
                {
                    tool.state = ToolState::Unanswered;
                }
                Kind::Agent(agent)
                    if matches!(agent.state, AgentState::Waiting | AgentState::Working) =>
                {
                    agent.state = AgentState::Stopped;
                }
                _ => continue,
            }
            entry.body = None;
        }
    }

    fn push(&mut self, kind: Kind) {
        self.close();
        self.entries.push(Entry::new(kind));
    }

    /// Ends the text or thinking being written.
    fn close(&mut self) {
        if let Some(entry) = self.entries.last_mut()
            && let Kind::Reply { open, .. } | Kind::Thinking { open, .. } = &mut entry.kind
        {
            *open = false;
        }
    }

    fn text(&mut self, text: &str) {
        match self.entries.last_mut() {
            Some(
                entry @ Entry {
                    kind: Kind::Reply { open: true, .. },
                    ..
                },
            ) => {
                if let Kind::Reply { text: written, .. } = &mut entry.kind {
                    written.push_str(text);
                }
                entry.body = None;
            }
            _ if text.trim().is_empty() => {}
            _ => self.push(Kind::Reply {
                text: text.to_owned(),
                open: true,
            }),
        }
    }

    fn reasoning(&mut self, text: &str) {
        match self.entries.last_mut() {
            Some(
                entry @ Entry {
                    kind: Kind::Thinking { open: true, .. },
                    ..
                },
            ) => {
                if let Kind::Thinking { text: written, .. } = &mut entry.kind {
                    written.push_str(text);
                }
                entry.body = None;
            }
            _ => self.push(Kind::Thinking {
                text: text.to_owned(),
                took: None,
                open: true,
            }),
        }
    }

    /// Reasoning ended after `duration`: that of the thinking block open, or
    /// of reasoning whose text the provider withholds.
    fn thought(&mut self, duration: Duration) {
        match self.entries.last_mut() {
            Some(
                entry @ Entry {
                    kind: Kind::Thinking { open: true, .. },
                    ..
                },
            ) => {
                if let Kind::Thinking { took, open, .. } = &mut entry.kind {
                    *took = Some(duration);
                    *open = false;
                }
            }
            _ => self.push(Kind::Thinking {
                text: String::new(),
                took: Some(duration),
                open: false,
            }),
        }
    }

    fn call(&mut self, name: &str, args: &str, machine: Option<String>, call: Option<CallId>) {
        let machine = machine.or_else(|| render::call_machine(args));
        let written = self.entries.iter().position(|entry| {
            matches!(&entry.kind, Kind::Tool(tool) if matches!(tool.state, ToolState::Writing) && tool.name == name)
        });
        if name == subagents::TOOL {
            let cards = spawned(args, call.as_ref())
                .into_iter()
                .map(|agent| Entry::new(Kind::Agent(agent)));
            match written {
                Some(at) => _ = self.entries.splice(at..=at, cards),
                None => {
                    self.close();
                    self.entries.extend(cards);
                }
            }
            return;
        }
        let tool = Tool {
            name: name.to_owned(),
            args: args.to_owned(),
            machine,
            call,
            output: String::new(),
            state: ToolState::Running,
        };
        match written {
            Some(at) => self.entries[at] = Entry::new(Kind::Tool(tool)),
            None => self.push(Kind::Tool(tool)),
        }
    }

    fn result(&mut self, result: &ToolResult) {
        let text = render::result_text(result);
        let called = |tool: &Tool| tool.call.as_ref() == Some(&result.call);
        if let Some(at) = self.last_tool(called) {
            let entry = &mut self.entries[at];
            if let Kind::Tool(tool) = &mut entry.kind {
                tool.state = match result.is_error {
                    true => ToolState::Failed(text),
                    false => ToolState::Done(text),
                };
            }
            entry.body = None;
            return;
        }
        let reports: Vec<Report> = serde_json::from_str(&text).unwrap_or_default();
        for entry in &mut self.entries {
            let Kind::Agent(card) = &mut entry.kind else {
                continue;
            };
            let Some((_, task)) = card.spawn.as_ref().filter(|(call, _)| *call == result.call)
            else {
                continue;
            };
            card.state = match reports.get(*task) {
                Some(Report {
                    error: Some(error),
                    agent,
                    ..
                }) => {
                    card.agent = Some(agent.clone());
                    AgentState::Failed(error.clone())
                }
                Some(Report { output, agent, .. }) => {
                    card.agent = Some(agent.clone());
                    AgentState::Done(output.clone().unwrap_or_default())
                }
                None if result.is_error => AgentState::Failed(text.clone()),
                // The call ended, but without a report this card can read.
                None => AgentState::Done(String::new()),
            };
            entry.body = None;
        }
    }

    /// The card of a task `agent` began: the one that named it, else the
    /// first waiting under `label`, else a card of its own.
    fn agent_started(&mut self, agent: &AgentId, label: &str) {
        let waiting = |card: &Agent, named: bool| {
            matches!(card.state, AgentState::Waiting)
                && match named {
                    true => card.agent.as_ref() == Some(agent),
                    false => card.agent.is_none() && card.label == label,
                }
        };
        let found = [true, false].into_iter().find_map(|named| {
            self.entries
                .iter()
                .rposition(|entry| matches!(&entry.kind, Kind::Agent(card) if waiting(card, named)))
        });
        match found {
            Some(at) => {
                let entry = &mut self.entries[at];
                if let Kind::Agent(card) = &mut entry.kind {
                    card.agent = Some(agent.clone());
                    card.state = AgentState::Working;
                }
                entry.body = None;
            }
            None => self.push(Kind::Agent(Agent {
                agent: Some(agent.clone()),
                label: label.to_owned(),
                message: String::new(),
                spawn: None,
                steps: Vec::new(),
                state: AgentState::Working,
            })),
        }
    }

    fn with_agent(&mut self, agent: &AgentId, change: impl FnOnce(&mut Agent)) {
        let entry = self.entries.iter_mut().rev().find(
            |entry| matches!(&entry.kind, Kind::Agent(card) if card.agent.as_ref() == Some(agent)),
        );
        if let Some(entry) = entry {
            if let Kind::Agent(card) = &mut entry.kind {
                change(card);
            }
            entry.body = None;
        }
    }

    fn tools(&mut self) -> impl DoubleEndedIterator<Item = &mut Tool> {
        self.entries
            .iter_mut()
            .filter_map(|entry| match &mut entry.kind {
                Kind::Tool(tool) => Some(tool),
                _ => None,
            })
    }

    fn touch_last_tool(&mut self) {
        if let Some(at) = self.last_tool(|_| true) {
            self.entries[at].body = None;
        }
    }

    fn last_tool(&self, matching: impl Fn(&Tool) -> bool) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(&entry.kind, Kind::Tool(tool) if matching(tool)))
    }
}

/// The cards of a `spawn_agents` call's tasks, waiting to start.
fn spawned(args: &str, call: Option<&CallId>) -> Vec<Agent> {
    let tasks =
        serde_json::from_str::<Spawned>(args).map_or_else(|_| Vec::new(), |spawned| spawned.agents);
    tasks
        .into_iter()
        .enumerate()
        .map(|(at, task)| Agent {
            agent: task.agent,
            label: task.label,
            message: task.message,
            spawn: call.map(|call| (call.clone(), at)),
            steps: Vec::new(),
            state: AgentState::Waiting,
        })
        .collect()
}

/// What a subagent did, as a line under its card.
fn step_line(step: &Event) -> Option<String> {
    let line = match step {
        Event::ToolCall {
            name,
            args,
            machine,
            ..
        } => {
            let on = machine.clone().or_else(|| render::call_machine(args));
            let on = on
                .map(|machine| format!(" on {machine}"))
                .unwrap_or_default();
            format!("{name}{on} {}", render::call_summary(args))
        }
        Event::Notice { text } => text.clone(),
        _ => return None,
    };
    Some(line.replace('\n', " "))
}

impl Entry {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            expanded: false,
            body: None,
        }
    }

    /// Whether it has more to show unfolded than folded.
    pub fn expandable(&self) -> bool {
        match &self.kind {
            Kind::Thinking { text, .. } => !text.trim().is_empty(),
            Kind::Tool(_) | Kind::Agent(_) | Kind::Compacted { .. } => true,
            Kind::User(_) | Kind::Reply { .. } | Kind::Notice(_) | Kind::Error(_) => false,
        }
    }

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Whether it shows as a single line, which others of its kind follow
    /// without a gap.
    pub fn compact(&self) -> bool {
        match &self.kind {
            Kind::Tool(_) | Kind::Agent(_) | Kind::Compacted { .. } => !self.expanded,
            Kind::Notice(_) => true,
            Kind::User(_) | Kind::Reply { .. } | Kind::Thinking { .. } | Kind::Error(_) => false,
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded && self.expandable();
    }

    /// Its rows, none wider than `width`; `spinner` is the frame shown for
    /// what is in progress.
    pub fn rows(&mut self, width: usize, style: &Style, spinner: &str) -> Vec<String> {
        let header = self.header(width, spinner);
        header
            .into_iter()
            .chain(self.body(width, style).iter().cloned())
            .collect()
    }

    fn header(&self, width: usize, spinner: &str) -> Option<String> {
        let palette = palette();
        let header = match &self.kind {
            Kind::User(_) | Kind::Reply { .. } | Kind::Notice(_) | Kind::Error(_) => return None,
            Kind::Thinking { open: true, .. } => {
                format!(
                    "{} {}",
                    palette.accent.paint(spinner),
                    palette.muted.paint("Thinking").italic()
                )
            }
            Kind::Thinking { took, text, .. } => {
                let hidden = match self.expanded {
                    true => 0,
                    false => thinking_rows(text, inner(width))
                        .len()
                        .saturating_sub(PREVIEW_ROWS),
                };
                let more = match hidden {
                    0 => String::new(),
                    1 => " · 1 more line".to_owned(),
                    hidden => format!(" · {hidden} more lines"),
                };
                let said = render::clip(&render::thought_text(*took), inner(width));
                format!(
                    "{} {}{}",
                    palette.muted.paint("◇"),
                    palette.muted.paint(&said).italic(),
                    palette.muted.paint(&more)
                )
            }
            Kind::Tool(tool) => tool.header(width, spinner),
            Kind::Agent(agent) => agent.header(width, spinner),
            Kind::Compacted { sizes, .. } => {
                let sizes = sizes.map(|(before, after)| {
                    format!(
                        " ~{} → ~{} tokens",
                        render::tokens(before),
                        render::tokens(after)
                    )
                });
                format!(
                    "{} {}{}",
                    palette.name.paint("◇"),
                    "Compacted the conversation".bold(),
                    palette.muted.paint(&sizes.unwrap_or_default())
                )
            }
        };
        Some(header)
    }

    fn body(&mut self, width: usize, style: &Style) -> &[String] {
        let fresh = self
            .body
            .as_ref()
            .is_some_and(|body| body.width == width && body.expanded == self.expanded);
        if !fresh {
            let rows = self.draw_body(width, style);
            self.body = Some(Body {
                width,
                expanded: self.expanded,
                rows,
            });
        }
        self.body.as_ref().map_or(&[], |body| &body.rows)
    }

    fn draw_body(&self, width: usize, style: &Style) -> Vec<String> {
        let palette = palette();
        match &self.kind {
            Kind::User(text) => render::wrapped(text.trim(), inner(width))
                .into_iter()
                .enumerate()
                .map(|(at, row)| {
                    let lead = if at == 0 { "> " } else { "  " };
                    format!(
                        "{}{}",
                        palette.accent.paint(lead),
                        palette.input.paint(&row)
                    )
                })
                .collect(),
            Kind::Reply { text, .. } => markdown(text, width, style),
            Kind::Thinking { text, .. } => {
                let rows = thinking_rows(text, inner(width));
                let shown = match self.expanded {
                    true => &rows[..],
                    false => &rows[rows.len().saturating_sub(PREVIEW_ROWS)..],
                };
                shown
                    .iter()
                    .map(|row| gutter(&palette.muted.paint(row).italic().to_string()))
                    .collect()
            }
            Kind::Tool(tool) if self.expanded => tool.body(inner(width)),
            Kind::Agent(agent) if self.expanded => agent.body(inner(width), style),
            Kind::Compacted { summary, .. } if self.expanded => {
                markdown(summary, inner(width), style)
                    .iter()
                    .map(|row| gutter(row))
                    .collect()
            }
            Kind::Tool(_) | Kind::Agent(_) | Kind::Compacted { .. } => Vec::new(),
            Kind::Notice(text) => render::wrapped(text, inner(width))
                .iter()
                .map(|row| format!("  {}", palette.muted.paint(row)))
                .collect(),
            Kind::Error(text) => render::wrapped(&format!("Error: {text}"), width)
                .iter()
                .map(|row| row.red().to_string())
                .collect(),
        }
    }
}

impl Tool {
    fn header(&self, width: usize, spinner: &str) -> String {
        let palette = palette();
        let glyph = match &self.state {
            ToolState::Writing | ToolState::Running => palette.accent.paint(spinner).to_string(),
            ToolState::Done(_) => palette.name.paint("●").to_string(),
            ToolState::Failed(_) => "✗".red().to_string(),
            ToolState::Unanswered => palette.muted.paint("○").to_string(),
        };
        let outcome = match &self.state {
            ToolState::Done(text) => match render::changed_lines(&self.name, &self.args) {
                Some(lines) => {
                    let added = lines.iter().filter(|(mark, _)| *mark == '+').count();
                    format!("+{added} -{}", lines.len() - added)
                }
                None => render::result_summary(text),
            },
            ToolState::Failed(text) => text.lines().next().unwrap_or("failed").to_owned(),
            ToolState::Unanswered => "no result".to_owned(),
            ToolState::Writing => "writing".to_owned(),
            ToolState::Running => String::new(),
        };
        let on = self
            .machine
            .as_ref()
            .map(|machine| format!(" on {machine}"))
            .unwrap_or_default();
        let summary = render::call_summary(&self.args).replace(['\n', '\t'], " ");
        let room = width.saturating_sub(2 + self.name.chars().count() + on.chars().count() + 1);
        let outcome = render::clip(&outcome, room / 2);
        let summary = render::clip(&summary, room.saturating_sub(outcome.chars().count() + 3));
        let outcome = match (&self.state, outcome.is_empty()) {
            (_, true) => String::new(),
            (ToolState::Failed(_), false) => format!(" · {outcome}").red().to_string(),
            (_, false) => palette.muted.paint(&format!(" · {outcome}")).to_string(),
        };
        format!(
            "{glyph} {}{} {}{outcome}",
            self.name.as_str().bold(),
            palette.muted.paint(&on),
            palette.muted.paint(&summary)
        )
    }

    fn body(&self, width: usize) -> Vec<String> {
        let palette = palette();
        let muted = |text: &str| {
            render::wrapped(text, width)
                .iter()
                .map(|row| gutter(&palette.muted.paint(row).to_string()))
                .collect::<Vec<_>>()
        };
        let plain = |text: &str| {
            render::wrapped(text, width)
                .iter()
                .map(|row| gutter(row))
                .collect::<Vec<_>>()
        };
        let mut rows = Vec::new();
        if let ToolState::Writing = self.state {
            return muted(&self.args);
        }
        match (
            render::changed_lines(&self.name, &self.args),
            render::todo_rows(&self.name, &self.args),
        ) {
            (Some(lines), _) => {
                for (mark, line) in lines {
                    let row = render::clip(&format!("{mark} {line}"), width);
                    rows.push(gutter(&match mark {
                        '-' => row.red().to_string(),
                        _ => row.green().to_string(),
                    }));
                }
            }
            (None, Some(todos)) => rows.extend(todos.iter().map(|todo| gutter(todo.trim_start()))),
            (None, None) => match self.name.as_str() {
                "shell" => rows.extend(muted(&format!("$ {}", render::call_summary(&self.args)))),
                _ => rows.extend(muted(&render::call_input(&self.args))),
            },
        }
        let given = match &self.state {
            ToolState::Done(text) if !render::shows_changes(&self.name) => text.as_str(),
            ToolState::Done(_) => "",
            ToolState::Running | ToolState::Unanswered => self.output.as_str(),
            ToolState::Failed(text) => {
                rows.extend(
                    render::wrapped(text, width)
                        .iter()
                        .map(|row| gutter(&row.red().to_string())),
                );
                return rows;
            }
            ToolState::Writing => "",
        };
        if !given.trim().is_empty() {
            rows.push(gutter(
                &palette.muted.paint(&"─".repeat(width.min(40))).to_string(),
            ));
            rows.extend(plain(given.trim_end()));
        }
        rows
    }
}

impl Agent {
    fn header(&self, width: usize, spinner: &str) -> String {
        let palette = palette();
        let (glyph, said) = match &self.state {
            AgentState::Waiting => (palette.muted.paint("◇").to_string(), "waiting".to_owned()),
            AgentState::Working => (
                palette.accent.paint(spinner).to_string(),
                format!("{} steps", self.steps.len()),
            ),
            AgentState::Done(_) => (palette.name.paint("◆").to_string(), "done".to_owned()),
            AgentState::Failed(error) => (
                "✗".red().to_string(),
                error.lines().next().unwrap_or("failed").to_owned(),
            ),
            AgentState::Stopped => (palette.muted.paint("◇").to_string(), "stopped".to_owned()),
        };
        let id = self
            .agent
            .as_ref()
            .map(|agent| format!(" · agent {agent}"))
            .unwrap_or_default();
        let room = width.saturating_sub(2 + id.chars().count() + 3);
        let said = render::clip(&said, room / 2);
        let label = render::clip(&self.label, room.saturating_sub(said.chars().count()));
        let said = match self.state {
            AgentState::Failed(_) => format!(" · {said}").red().to_string(),
            _ => palette.muted.paint(&format!(" · {said}")).to_string(),
        };
        format!("{glyph} {}{}{said}", label.bold(), palette.muted.paint(&id))
    }

    fn body(&self, width: usize, style: &Style) -> Vec<String> {
        let palette = palette();
        let mut rows: Vec<String> = render::wrapped(self.message.trim(), width)
            .iter()
            .map(|row| gutter(&palette.input.paint(row).to_string()))
            .collect();
        let skipped = self.steps.len().saturating_sub(AGENT_STEPS);
        if skipped > 0 {
            rows.push(gutter(
                &palette
                    .muted
                    .paint(&format!("… {skipped} earlier steps"))
                    .to_string(),
            ));
        }
        for step in &self.steps[skipped..] {
            rows.push(gutter(
                &palette
                    .muted
                    .paint(&render::clip(&format!("› {step}"), width))
                    .to_string(),
            ));
        }
        match &self.state {
            AgentState::Done(output) if !output.trim().is_empty() => {
                rows.push(gutter(
                    &palette.muted.paint(&"─".repeat(width.min(40))).to_string(),
                ));
                rows.extend(markdown(output, width, style).iter().map(|row| gutter(row)));
            }
            AgentState::Failed(error) => {
                rows.extend(
                    render::wrapped(error, width)
                        .iter()
                        .map(|row| gutter(&row.red().to_string())),
                );
            }
            _ => {}
        }
        if self.agent.is_some() {
            rows.push(gutter(
                &palette.muted.paint("→ opens its conversation").to_string(),
            ));
        }
        rows
    }
}

/// `text` as a reply shows it: Markdown highlighted when that is on,
/// wrapped as replies are.
fn markdown(text: &str, width: usize, style: &Style) -> Vec<String> {
    let width = style.wrap.map_or(width, |wrap| wrap.min(width));
    let text = text.trim_matches('\n');
    match style.highlight {
        true => render::reply_rows(text, render::current_theme(), width),
        false => render::wrapped(text, width),
    }
}

/// Reasoning's rows, without the blank lines between its paragraphs.
fn thinking_rows(text: &str, width: usize) -> Vec<String> {
    render::wrapped(text.trim(), width)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect()
}

/// A row set under the header of what it belongs to.
fn gutter(row: &str) -> String {
    format!("{} {row}", palette().muted.paint("│"))
}

/// Columns a row has under a header, beside the gutter.
fn inner(width: usize) -> usize {
    width.saturating_sub(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(transcript: &Transcript) -> Vec<&'static str> {
        transcript
            .entries
            .iter()
            .map(|entry| match &entry.kind {
                Kind::User(_) => "user",
                Kind::Reply { .. } => "reply",
                Kind::Thinking { .. } => "thinking",
                Kind::Tool(_) => "tool",
                Kind::Agent(_) => "agent",
                Kind::Compacted { .. } => "compacted",
                Kind::Notice(_) => "notice",
                Kind::Error(_) => "error",
            })
            .collect()
    }

    #[test]
    fn a_run_becomes_entries_and_subagents_cards_of_their_own() {
        let mut transcript = Transcript::default();
        transcript.user("look into it");
        let spawn = r#"{"agents":[{"label":"find","message":"find it"},{"label":"read","message":"read it"}]}"#;
        let events = [
            Event::Reasoning { text: "hm".into() },
            Event::Thought {
                duration: Duration::from_secs(2),
            },
            Event::Text {
                text: "Let me ".into(),
            },
            Event::Text {
                text: "look.".into(),
            },
            Event::PartEnd,
            Event::ToolCallStart {
                name: subagents::TOOL.into(),
            },
            Event::ToolCallArguments { json: spawn.into() },
            Event::ToolCall {
                name: subagents::TOOL.into(),
                args: spawn.into(),
                machine: None,
                call: Some(CallId::from_wire("c1")),
            },
            Event::AgentStarted {
                agent: AgentId("a1".into()),
                label: "find".into(),
            },
            Event::AgentStarted {
                agent: AgentId("a2".into()),
                label: "read".into(),
            },
            Event::Agent {
                agent: AgentId("a2".into()),
                step: Box::new(Event::Notice {
                    text: "busy".into(),
                }),
            },
        ];
        events.iter().for_each(|event| transcript.advance(event));
        assert_eq!(
            kinds(&transcript),
            ["user", "thinking", "reply", "agent", "agent"]
        );
        assert_eq!(transcript.agent(3), Some(&AgentId("a1".into())));
        let Kind::Agent(read) = &transcript.entries[4].kind else {
            unreachable!()
        };
        assert_eq!(read.steps, ["busy"]);
        let Kind::Reply { text, .. } = &transcript.entries[2].kind else {
            unreachable!()
        };
        assert_eq!(text, "Let me look.");

        transcript.advance(&Event::End {
            ending: Ending::Interrupted,
            saved: 0,
        });
        let Kind::Agent(find) = &transcript.entries[3].kind else {
            unreachable!()
        };
        assert!(matches!(find.state, AgentState::Stopped));
        assert_eq!(kinds(&transcript).last(), Some(&"notice"));
    }

    #[test]
    fn thinking_previews_its_latest_rows_and_tool_calls_fold_to_a_line() {
        yansi::disable();
        let style = Style {
            highlight: false,
            wrap: None,
            thinking_output: crate::config::Output::Preview,
            tool_output: crate::config::Output::Preview,
            subagent_output: crate::config::Output::Preview,
            notify_after: None,
        };
        let mut transcript = Transcript::default();
        transcript.advance(&Event::Reasoning {
            text: "1\n\n2\n3\n4\n5\n6".into(),
        });
        transcript.advance(&Event::Thought {
            duration: Duration::from_secs(3),
        });
        transcript.advance(&Event::ToolCall {
            name: "shell".into(),
            args: r#"{"command":"ls"}"#.into(),
            machine: None,
            call: Some(CallId::from_wire("c")),
        });
        let thinking = transcript.entries[0].rows(40, &style, "*");
        assert_eq!(
            thinking,
            [
                "◇ Thought for 3s · 2 more lines",
                "│ 3",
                "│ 4",
                "│ 5",
                "│ 6"
            ]
        );
        assert_eq!(transcript.entries[1].rows(40, &style, "*"), ["* shell ls"]);

        transcript.entries[1].set_expanded(true);
        assert_eq!(
            transcript.entries[1].rows(40, &style, "*"),
            ["* shell ls", "│ $ ls"]
        );
        transcript.entries[0].set_expanded(true);
        assert_eq!(
            transcript.entries[0].rows(40, &style, "*").len(),
            7,
            "all six lines"
        );

        let mut long = Transcript::default();
        long.advance(&Event::Reasoning {
            text: "word ".repeat(60),
        });
        long.advance(&Event::Thought {
            duration: Duration::from_secs(1),
        });
        let rows = long.entries[0].rows(40, &style, "*");
        assert_eq!(rows.len(), 1 + PREVIEW_ROWS);
        assert!(rows[0].ends_with("more lines"), "{rows:?}");
    }
}
