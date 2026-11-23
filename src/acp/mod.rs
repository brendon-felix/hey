/* -------------------------------------------------------------------------- */
/*                                   acp.rs                                   */
/* -------------------------------------------------------------------------- */

//! `hey --acp`: the Agent Client Protocol over stdio, so an editor such as
//! Zed can host hey as an external agent. Each ACP session is one hey
//! conversation, answered by the same host as the terminal's; hey's events
//! become the protocol's updates, and what would ask in the terminal asks
//! the editor, as a permission request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AvailableCommandsUpdate, CancelNotification, Content, ContentBlock,
    ContentChunk, ImageContent, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionAdditionalDirectoriesCapabilities,
    SessionCapabilities, SessionInfo, SessionListCapabilities, SessionNotification, SessionUpdate,
    StopReason, TextContent, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, Client, Error, Stdio};
use anyhow::Result;
use futures::StreamExt;
use rig_core::message::ImageMediaType;

use crate::backend::Backend;
use crate::config::Thinking;
use crate::protocol::{Ask, Attachment, Decision, Ending, Event, RunOptions};
use crate::sessions::SessionId;
use crate::turn;

mod commands;

/// A conversation the editor opened, and the folders of its workspace.
#[derive(Clone)]
struct Session {
    thread: SessionId,
    /// The workspace's folders, the first being the working directory; told
    /// to the model with the first prompt.
    roots: Vec<PathBuf>,
    told: bool,
    /// What `/model`, `/think`, `/goal` and `/plan` set for this session.
    model: String,
    thinking: Option<Thinking>,
    goal: Option<String>,
    plan: bool,
}

/// What every session answers with.
struct Shared {
    backend: Backend,
    /// What a session starts with.
    model: String,
    thinking: Option<Thinking>,
    system_prompt: Option<String>,
    /// The hey conversation of each ACP session, by the id the editor knows.
    sessions: Mutex<HashMap<String, Session>>,
}

/// Serves ACP on stdin and stdout until the editor closes them. Nothing
/// else may write to stdout.
pub async fn serve(
    backend: Backend,
    model: String,
    thinking: Option<Thinking>,
    system_prompt: Option<String>,
) -> Result<()> {
    let shared = Arc::new(Shared {
        backend,
        model,
        thinking,
        system_prompt,
        sessions: Mutex::new(HashMap::new()),
    });
    let on_new = shared.clone();
    let on_prompt = shared.clone();
    let on_cancel = shared.clone();
    let on_load = shared.clone();
    let on_list = shared.clone();
    let served = Agent
        .v1()
        .name("hey")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                let capabilities = AgentCapabilities::new()
                    .load_session(true)
                    .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                    .session_capabilities(SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .additional_directories(SessionAdditionalDirectoriesCapabilities::new()),
                    );
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(capabilities)
                        .agent_info(Implementation::new("hey", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                // Tools and instructions work from the process's directory.
                if let Err(error) = std::env::set_current_dir(&request.cwd) {
                    tracing::warn!(cwd = %request.cwd.display(), %error, "cannot enter the session's directory");
                }
                let thread = SessionId::new();
                let id = thread.as_str().to_owned();
                let roots = std::iter::once(request.cwd.clone())
                    .chain(request.additional_directories.clone())
                    .collect();
                let session = Session {
                    thread,
                    roots,
                    told: false,
                    model: on_new.model.clone(),
                    thinking: on_new.thinking,
                    goal: None,
                    plan: false,
                };
                on_new.sessions.lock().unwrap().insert(id.clone(), session);
                responder.respond(NewSessionResponse::new(id.clone()))?;
                // After the response, which the editor needs to know the session.
                cx.send_notification(SessionNotification::new(
                    id,
                    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                        commands::available(&on_new).await,
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                if let Err(error) = std::env::set_current_dir(&request.cwd) {
                    tracing::warn!(cwd = %request.cwd.display(), %error, "cannot enter the session's directory");
                }
                let id = request.session_id.0.to_string();
                let Some(thread) = on_load.backend.threads().await.ok().and_then(|threads| {
                    threads
                        .into_iter()
                        .find(|info| info.summary.id.as_str() == id)
                        .map(|info| info.summary.id)
                }) else {
                    return responder.respond_with_error(Error::invalid_params().data(format!("unknown session {id}")));
                };
                let messages = match on_load.backend.view(&thread).await {
                    Ok(opened) => opened.snapshot.messages,
                    Err(error) => {
                        return responder
                            .respond_with_error(Error::internal_error().data(format!("{error:#}")));
                    }
                };
                let roots = std::iter::once(request.cwd.clone())
                    .chain(request.additional_directories.clone())
                    .collect();
                on_load.sessions.lock().unwrap().insert(
                    id.clone(),
                    Session {
                        thread,
                        roots,
                        told: true,
                        model: on_load.model.clone(),
                        thinking: on_load.thinking,
                        goal: None,
                        plan: false,
                    },
                );
                replay(&cx, &request.session_id, &messages);
                responder.respond(LoadSessionResponse::new())?;
                cx.send_notification(SessionNotification::new(
                    id,
                    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                        commands::available(&on_load).await,
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _cx| {
                let cwd = request
                    .cwd
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("/"));
                match on_list.backend.threads().await {
                    Ok(threads) => responder.respond(ListSessionsResponse::new(
                        threads
                            .into_iter()
                            .filter(|info| !info.archived)
                            .map(|info| {
                                SessionInfo::new(info.summary.id.as_str().to_owned(), cwd.clone())
                                    .title(info.summary.title)
                            })
                            .collect(),
                    )),
                    Err(error) => responder
                        .respond_with_error(Error::internal_error().data(format!("{error:#}"))),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let shared = on_prompt.clone();
                let task = cx.clone();
                // Off the dispatch loop, which must stay free for a cancel.
                cx.spawn(async move {
                    match prompt(&shared, &task, request).await {
                        Ok(reason) => responder.respond(PromptResponse::new(reason)),
                        Err(error) => {
                            tracing::error!(error = format!("{error:#}"), "prompt failed");
                            responder.respond_with_error(
                                Error::internal_error().data(format!("{error:#}")),
                            )
                        }
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |cancel: CancelNotification, _cx| {
                let thread = on_cancel
                    .sessions
                    .lock()
                    .unwrap()
                    .get(cancel.session_id.0.as_ref())
                    .map(|session| session.thread.clone());
                if let Some(thread) = thread {
                    on_cancel.backend.stop(&thread).await.ok();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;
    served.map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))
}

/// Sends a saved conversation to the editor as it happened.
fn replay(
    cx: &ConnectionTo<Client>,
    session: &agent_client_protocol::v1::schema::SessionId,
    messages: &[rig_core::message::Message],
) {
    use rig_core::message::{AssistantContent, Message, UserContent};
    let send = |update: SessionUpdate| {
        cx.send_notification(SessionNotification::new(session.clone(), update))
            .ok();
    };
    for message in messages {
        match message {
            Message::User { content } => {
                for part in content.iter() {
                    match part {
                        UserContent::Text(text) if !crate::mention::is_context(&text.text) => {
                            send(SessionUpdate::UserMessageChunk(chunk(text.text.clone())))
                        }
                        UserContent::ToolResult(result) => {
                            let status = match result.is_error {
                                true => ToolCallStatus::Failed,
                                false => ToolCallStatus::Completed,
                            };
                            let fields = ToolCallUpdateFields::new().status(status).content(vec![
                                ToolCallContent::Content(Content::new(ContentBlock::from(
                                    crate::render::result_text(result),
                                ))),
                            ]);
                            send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                result.call.wire().into_owned(),
                                fields,
                            )));
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant(assistant) => {
                for part in assistant.content.iter() {
                    match part {
                        AssistantContent::Text(text) => {
                            send(SessionUpdate::AgentMessageChunk(chunk(text.text.clone())))
                        }
                        AssistantContent::Reasoning(reasoning) => send(
                            SessionUpdate::AgentThoughtChunk(chunk(reasoning.text.clone())),
                        ),
                        AssistantContent::ToolCall(call) => {
                            let name = call.function.name.as_str();
                            let input = serde_json::Value::Object(call.function.arguments.clone());
                            send(SessionUpdate::ToolCall(
                                ToolCall::new(
                                    call.id.wire().into_owned(),
                                    title(name, Some(&input)),
                                )
                                .name(name.to_owned())
                                .kind(kind(name))
                                .status(ToolCallStatus::InProgress)
                                .raw_input(Some(input)),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Answers `request`: a command it types, or a message for the model.
async fn prompt(
    shared: &Shared,
    cx: &ConnectionTo<Client>,
    request: PromptRequest,
) -> Result<StopReason> {
    let session = request.session_id.clone();
    let key = session.0.to_string();
    let (text, attachments) = content(request.prompt);
    let what = match commands::run(shared, &key, &text).await? {
        commands::Outcome::Said(said) => {
            cx.send_notification(SessionNotification::new(
                session.clone(),
                SessionUpdate::AgentMessageChunk(chunk(said)),
            ))
            .ok();
            return Ok(StopReason::EndTurn);
        }
        commands::Outcome::Ask(text) => commands::Run::Answer(text),
        commands::Outcome::Run(run) => run,
    };
    let (thread, run) = {
        let mut sessions = shared.sessions.lock().unwrap();
        let known = sessions
            .get_mut(&key)
            .ok_or_else(|| anyhow::anyhow!("unknown session {key}"))?;
        let options = RunOptions {
            model: known.model.clone(),
            thinking: known.thinking,
            system_prompt: shared.system_prompt.clone(),
        };
        let mut run = match what {
            commands::Run::Compact(focus) => turn::compaction(focus, options),
            commands::Run::Answer(mut text) => {
                // A slash command invokes a skill only at the message's start,
                // so the workspace waits for a message that has none.
                if !text.trim_start().starts_with('/')
                    && !std::mem::replace(&mut known.told, true)
                    && let Some(note) = workspace(&known.roots)
                {
                    text = format!("{note}\n\n{text}");
                }
                let mut run = turn::request(text.clone(), options);
                run.attachments.extend(attachments);
                run.prompt = text;
                run.goal = known.goal.clone();
                run
            }
        };
        run.client = None;
        (known.thread.clone(), run)
    };
    let run = run;
    let mut events = shared.backend.start(&thread, run).await?;
    // With a remote host, the calls it lends this machine run here.
    let mut follower = None;
    let mut lent = tokio::task::JoinSet::new();
    let send = |update: SessionUpdate| {
        cx.send_notification(SessionNotification::new(session.clone(), update))
            .ok();
    };
    // The calls shown and not yet answered, oldest first, by rig's call id.
    let mut open: Vec<(String, String)> = Vec::new();
    let mut counter = 0u64;
    loop {
        let Some(event) = events.next().await else {
            anyhow::bail!("the run's events stopped before it ended");
        };
        match event? {
            Event::Text { text } => send(SessionUpdate::AgentMessageChunk(chunk(text))),
            Event::Reasoning { text } => send(SessionUpdate::AgentThoughtChunk(chunk(text))),
            Event::ToolCall {
                name, args, call, ..
            } => {
                counter += 1;
                let id = call
                    .map(|call| call.wire().into_owned())
                    .unwrap_or_else(|| format!("call-{counter}"));
                let input = serde_json::from_str(&args).ok();
                let title = title(&name, input.as_ref());
                open.push((id.clone(), title.clone()));
                send(SessionUpdate::ToolCall(
                    ToolCall::new(id, title)
                        .name(name.clone())
                        .kind(kind(&name))
                        .status(ToolCallStatus::InProgress)
                        .raw_input(input),
                ));
            }
            Event::ToolResult { result } => {
                let id = result.call.wire().into_owned();
                open.retain(|(open, _)| *open != id);
                let status = match result.is_error {
                    true => ToolCallStatus::Failed,
                    false => ToolCallStatus::Completed,
                };
                let text = crate::render::result_text(&result);
                let fields = ToolCallUpdateFields::new().status(status).content(vec![
                    ToolCallContent::Content(Content::new(ContentBlock::from(text))),
                ]);
                send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id, fields,
                )));
            }
            Event::Began {
                follower: began, ..
            } => follower = began,
            Event::ClientCall { id, call } => {
                if let Some(call) =
                    follower.and_then(|follower| shared.backend.lend(&thread, follower, id, call))
                {
                    lent.spawn(call);
                }
            }
            Event::Approval { id, ask } => {
                let decision = permission(cx, &session, open.last(), &ask).await;
                shared.backend.answer(&thread, id, decision).await?;
            }
            Event::Plan { on } => {
                if let Some(known) = shared.sessions.lock().unwrap().get_mut(&key) {
                    known.plan = on;
                }
            }
            Event::Compacted { before, after, .. } => {
                send(SessionUpdate::AgentMessageChunk(chunk(format!(
                    "Compacted the conversation: about {before} tokens became {after}.\n\n"
                ))))
            }
            Event::Notice { text } => tracing::info!(notice = %text, "hey notice"),
            Event::End { ending, .. } => {
                lent.shutdown().await;
                return match ending {
                    Ending::Answered => Ok(StopReason::EndTurn),
                    Ending::Interrupted => Ok(StopReason::Cancelled),
                    Ending::Failed { error } => Err(anyhow::anyhow!(error)),
                };
            }
            _ => {}
        }
    }
}

/// What tells the model of a workspace of several folders; nothing for one.
fn workspace(roots: &[PathBuf]) -> Option<String> {
    if roots.len() < 2 {
        return None;
    }
    let list: Vec<String> = roots
        .iter()
        .map(|root| format!("- {}", root.display()))
        .collect();
    Some(format!(
        "The editor's workspace has {} folders, all the user's project. The first, {}, is \
         your working directory; use absolute paths for the others:\n{}",
        roots.len(),
        roots[0].display(),
        list.join("\n")
    ))
}

fn chunk(text: String) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// How the editor labels a call: the tool, and what stands out of its
/// arguments.
fn title(name: &str, input: Option<&serde_json::Value>) -> String {
    let detail = input.and_then(|input| {
        ["command", "path", "pattern", "url", "query"]
            .iter()
            .find_map(|key| input.get(key).and_then(|value| value.as_str()))
    });
    match detail {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_owned(),
    }
}

fn kind(tool: &str) -> ToolKind {
    match tool {
        "read" | "list" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" | "web_search" => ToolKind::Search,
        "fetch" => ToolKind::Fetch,
        "shell" | "bash" | "spawn_process" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

/// The prompt's text, and what it carries besides: files the editor
/// embedded, images, and links to files, which become mentions.
fn content(blocks: Vec<ContentBlock>) -> (String, Vec<Attachment>) {
    let mut text = String::new();
    let mut attachments = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(block) => push(&mut text, &block.text),
            ContentBlock::ResourceLink(link) => match path(&link.uri) {
                Some(path) => push(&mut text, &format!("@{}", path.display())),
                None => push(&mut text, &link.uri),
            },
            ContentBlock::Resource(resource) => {
                use agent_client_protocol::v1::schema::EmbeddedResourceResource as Resource;
                if let Resource::TextResourceContents(file) = resource.resource {
                    let name =
                        path(&file.uri).map_or(file.uri.clone(), |path| path.display().to_string());
                    attachments.push(Attachment::File {
                        path: name,
                        text: file.text,
                    });
                }
            }
            ContentBlock::Image(ImageContent {
                data, mime_type, ..
            }) => attachments.push(Attachment::Image {
                path: "image".into(),
                media_type: media_type(&mime_type),
                data,
            }),
            _ => {}
        }
    }
    (text, attachments)
}

fn push(text: &mut String, more: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(more);
}

fn path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

fn media_type(mime: &str) -> Option<ImageMediaType> {
    match mime {
        "image/png" => Some(ImageMediaType::PNG),
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// Asks the editor to decide `ask`; anything but a choice is a refusal.
async fn permission(
    cx: &ConnectionTo<Client>,
    session: &agent_client_protocol::v1::schema::SessionId,
    call: Option<&(String, String)>,
    ask: &Ask,
) -> Decision {
    let option = |id: &str, name: &str, kind| PermissionOption::new(id.to_owned(), name, kind);
    let (title, options): (String, Vec<PermissionOption>) = match ask {
        Ask::Tool { name, .. } => (
            format!("Run `{name}`?"),
            vec![
                option("once", "Allow once", PermissionOptionKind::AllowOnce),
                option(
                    "always",
                    "Always allow in this conversation",
                    PermissionOptionKind::AllowAlways,
                ),
                option("decline", "Decline", PermissionOptionKind::RejectOnce),
            ],
        ),
        Ask::Directory { machine, path } => (
            format!("Let the agent use {path} on {machine}?"),
            vec![
                option(
                    "once",
                    "Allow in this conversation",
                    PermissionOptionKind::AllowOnce,
                ),
                option("decline", "Decline", PermissionOptionKind::RejectOnce),
            ],
        ),
        Ask::Question { question, choices } => (
            question.clone(),
            choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    option(
                        &format!("choice-{index}"),
                        choice,
                        PermissionOptionKind::AllowOnce,
                    )
                })
                .chain([option(
                    "decline",
                    "Decline to answer",
                    PermissionOptionKind::RejectOnce,
                )])
                .collect(),
        ),
        Ask::Plan { plan } => (
            format!("Approve this plan and leave plan mode?\n\n{plan}"),
            vec![
                option("once", "Yes, carry it out", PermissionOptionKind::AllowOnce),
                option(
                    "decline",
                    "No, keep planning",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        ),
    };
    let id = call.map_or("ask".to_owned(), |(id, _)| id.clone());
    let fields = ToolCallUpdateFields::new().title(title);
    let request =
        RequestPermissionRequest::new(session.clone(), ToolCallUpdate::new(id, fields), options);
    let outcome = match cx.send_request(request).block_task().await {
        Ok(RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(selected),
            ..
        }) => selected.option_id.0.to_string(),
        _ => return Decision::Stop,
    };
    match (outcome.as_str(), ask) {
        ("once", _) => Decision::Once,
        ("always", _) => Decision::Always,
        (choice, Ask::Question { choices, .. }) if choice.starts_with("choice-") => choice[7..]
            .parse::<usize>()
            .ok()
            .and_then(|index| choices.get(index))
            .map_or(Decision::Decline, |choice| Decision::Answer(choice.clone())),
        _ => Decision::Decline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_workspace_of_several_folders_is_told() {
        assert_eq!(workspace(&[PathBuf::from("/a")]), None);
        let note = workspace(&[PathBuf::from("/a"), PathBuf::from("/b")]).unwrap();
        assert!(note.contains("- /a") && note.contains("- /b"));
    }
}
