/* -------------------------------------------------------------------------- */
/*                                 server.rs                                  */
/* -------------------------------------------------------------------------- */

//! `hey --serve`: the host's operations over HTTP. Streams are
//! newline-delimited JSON, so a run can be followed with `curl -N`. A blank
//! line keeps a quiet stream open.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::{config, token};
use crate::host::{HEARTBEAT, Host, ThreadError};
use crate::protocol::{
    ApprovalId, Ask, CallId, CallResult, Decision, Event, FollowerId, MACHINE_HEADER, Revision,
    RunRequest,
};
use crate::sessions::SessionId;

pub async fn serve(host: Arc<Host>, config: &config::Server) -> Result<()> {
    let token: Arc<str> = token::host(&config.token)?.into();
    let app = Router::new()
        .route("/v1/defaults", get(defaults))
        .route("/v1/models", get(models))
        .route("/v1/model", get(model))
        .route("/v1/threads", get(threads))
        .route("/v1/activity", get(activity))
        .route("/v1/shared", get(shared))
        .route("/v1/skills", get(skills))
        .route("/v1/threads/{id}", get(open).delete(delete))
        .route("/v1/threads/{id}/run", post(start).delete(stop))
        .route("/v1/threads/{id}/viewed", post(viewed))
        .route("/v1/threads/{id}/archived", put(archive))
        .route("/v1/threads/{id}/fork", post(fork))
        .route("/v1/threads/{id}/approvals/{approval}", post(answer))
        .route("/v1/threads/{id}/calls/{call}", post(finish))
        .route("/v1/threads/{id}/calls/{call}/claim", post(claim))
        .route("/v1/threads/{id}/calls/{call}/events", post(call_event))
        .route("/v1/threads/{id}/calls/{call}/asks", post(call_ask))
        .route("/v1/threads/{id}/tools", get(tools))
        .route("/v1/memories", get(memories).post(remember).put(revise).delete(forget))
        .layer(middleware::from_fn_with_state(token, authorize))
        .with_state(host);
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.listen))?;
    println!("hey is hosting on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .await
        .context("the server stopped")
}

async fn authorize(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(presented) if same(presented.as_bytes(), token.as_bytes()) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "a valid token is required",
        )
            .into_response(),
    }
}

/// Compares in time independent of where the inputs differ, so the token
/// cannot be guessed a byte at a time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

type Reply<T> = Result<T, Failure>;

struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

async fn defaults(State(host): State<Arc<Host>>) -> impl IntoResponse {
    Json(host.defaults())
}

async fn models(State(host): State<Arc<Host>>) -> impl IntoResponse {
    Json(host.models())
}

#[derive(Deserialize)]
struct ModelQuery {
    reference: String,
}

async fn model(
    State(host): State<Arc<Host>>,
    Query(query): Query<ModelQuery>,
) -> Reply<impl IntoResponse> {
    let info = host
        .model(&query.reference)
        .map_err(|error| Failure(StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    Ok(Json(info))
}

async fn threads(State(host): State<Arc<Host>>) -> Reply<impl IntoResponse> {
    Ok(Json(host.threads()?))
}

/// What every thread is doing, a line each time that changes, and every
/// `HEARTBEAT` regardless.
async fn activity(State(host): State<Arc<Host>>) -> Response {
    ndjson_response(ndjson(host.activity()))
}

async fn shared(State(host): State<Arc<Host>>) -> impl IntoResponse {
    Json(host.shared())
}

async fn skills(State(host): State<Arc<Host>>) -> impl IntoResponse {
    Json(host.skills())
}

/// The machine a client that runs tool calls for the host names.
fn machine(headers: &HeaderMap) -> Option<String> {
    let name = headers.get(MACHINE_HEADER)?.to_str().ok()?;
    Some(name.to_owned()).filter(|name| !name.is_empty())
}

/// The thread's snapshot on the first line, then its run's events, if one is
/// in progress.
async fn open(
    State(host): State<Arc<Host>>,
    Path(id): Path<SessionId>,
    headers: HeaderMap,
) -> Reply<Response> {
    let opened = host.open(&id, machine(&headers))?;
    let snapshot = stream::once(async move { opened.snapshot });
    let lines = ndjson(snapshot).chain(ndjson(
        opened.events.unwrap_or_else(|| stream::empty().boxed()),
    ));
    Ok(ndjson_response(lines))
}

async fn start(
    State(host): State<Arc<Host>>,
    Path(id): Path<SessionId>,
    headers: HeaderMap,
    Json(request): Json<RunRequest>,
) -> Reply<Response> {
    let events = host.start(id, request, machine(&headers)).await.map_err(|error| match error {
        ThreadError::Busy => Failure(StatusCode::CONFLICT, error.to_string()),
        ThreadError::Other(_) => Failure(StatusCode::BAD_REQUEST, error.to_string()),
    })?;
    Ok(ndjson_response(ndjson(events)))
}

async fn stop(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> StatusCode {
    match host.stop(&id) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

async fn viewed(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> Reply<StatusCode> {
    host.viewed(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn archive(
    State(host): State<Arc<Host>>,
    Path(id): Path<SessionId>,
    Json(archived): Json<bool>,
) -> Reply<StatusCode> {
    host.archive(&id, archived)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fork(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> Reply<impl IntoResponse> {
    let copy = host
        .fork(&id)
        .map_err(|error| Failure(StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    Ok(Json(copy))
}

async fn delete(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> Reply<StatusCode> {
    host.delete(&id).map_err(|error| match error {
        ThreadError::Busy => Failure(StatusCode::CONFLICT, error.to_string()),
        ThreadError::Other(_) => Failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn answer(
    State(host): State<Arc<Host>>,
    Path((id, approval)): Path<(SessionId, u64)>,
    Json(decision): Json<Decision>,
) -> StatusCode {
    match host.answer(&id, ApprovalId(approval), decision) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::CONFLICT,
    }
}

async fn tools(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> impl IntoResponse {
    Json(host.tools(&id))
}

/// Takes up a call for the client's view of the run named in the body;
/// 409 when another took it, or it is no longer waited for.
async fn claim(
    State(host): State<Arc<Host>>,
    Path((id, call)): Path<(SessionId, u64)>,
    Json(follower): Json<FollowerId>,
) -> StatusCode {
    match host.claim(&id, CallId(call), follower) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::CONFLICT,
    }
}

async fn call_event(
    State(host): State<Arc<Host>>,
    Path((id, call)): Path<(SessionId, u64)>,
    Json(event): Json<Event>,
) -> StatusCode {
    match host.call_event(&id, CallId(call), event) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

/// The decision, once a client makes it.
async fn call_ask(
    State(host): State<Arc<Host>>,
    Path((id, call)): Path<(SessionId, u64)>,
    Json(ask): Json<Ask>,
) -> Reply<impl IntoResponse> {
    let decision = host
        .call_ask(&id, CallId(call), ask)
        .await
        .ok_or_else(|| Failure(StatusCode::NOT_FOUND, "the call is no longer waited for".into()))?;
    Ok(Json(decision))
}

async fn finish(
    State(host): State<Arc<Host>>,
    Path((id, call)): Path<(SessionId, u64)>,
    Json(result): Json<CallResult>,
) -> StatusCode {
    match host.finish(&id, CallId(call), result) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

async fn memories(State(host): State<Arc<Host>>) -> Reply<impl IntoResponse> {
    Ok(Json(host.memories()?))
}

async fn forget(State(host): State<Arc<Host>>, Json(memory): Json<String>) -> Reply<StatusCode> {
    Ok(match host.forget(&memory)? {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    })
}

/// 409 when it is already saved.
async fn remember(State(host): State<Arc<Host>>, Json(memory): Json<String>) -> Reply<StatusCode> {
    Ok(match host.remember(&memory)? {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::CONFLICT,
    })
}

async fn revise(State(host): State<Arc<Host>>, Json(revision): Json<Revision>) -> Reply<StatusCode> {
    host.revise(&revision)?;
    Ok(StatusCode::NO_CONTENT)
}

fn ndjson<T: Serialize>(
    items: impl Stream<Item = T> + Send + 'static,
) -> impl Stream<Item = serde_json::Result<String>> + Send + 'static {
    items.map(|item| serde_json::to_string(&item).map(|line| line + "\n"))
}

fn ndjson_response(
    lines: impl Stream<Item = serde_json::Result<String>> + Send + 'static,
) -> Response {
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(kept_open(lines)),
    )
        .into_response()
}

/// `lines`, with a blank line whenever they pause for a `HEARTBEAT`: proxies
/// such as Cloudflare's close a response that is silent for 100 seconds, as
/// a run is during a long tool call.
fn kept_open(
    lines: impl Stream<Item = serde_json::Result<String>> + Send + 'static,
) -> impl Stream<Item = serde_json::Result<String>> + Send + 'static {
    stream::unfold(lines.boxed(), |mut lines| async move {
        let line = tokio::select! {
            line = lines.next() => line?,
            () = tokio::time::sleep(HEARTBEAT) => Ok("\n".to_owned()),
        };
        Some((line, lines))
    })
}
