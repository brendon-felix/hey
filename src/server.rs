/* -------------------------------------------------------------------------- */
/*                                 server.rs                                  */
/* -------------------------------------------------------------------------- */

//! `hey --serve`: the host's operations over HTTP. Streams are
//! newline-delimited JSON, so a run can be followed with `curl -N`.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::host::{Host, StartError};
use crate::protocol::{ApprovalId, Decision, RunRequest};
use crate::sessions::SessionId;

pub async fn serve(host: Arc<Host>, config: &config::Server) -> Result<()> {
    let token: Arc<str> = config.token()?.into();
    let app = Router::new()
        .route("/v1/defaults", get(defaults))
        .route("/v1/models", get(models))
        .route("/v1/model", get(model))
        .route("/v1/threads", get(threads))
        .route("/v1/shared", get(shared))
        .route("/v1/threads/{id}", get(open))
        .route("/v1/threads/{id}/run", post(start).delete(stop))
        .route("/v1/threads/{id}/approvals/{approval}", post(answer))
        .route("/v1/threads/{id}/tools", get(tools))
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
        _ => (StatusCode::UNAUTHORIZED, "a valid token is required").into_response(),
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

async fn shared(State(host): State<Arc<Host>>) -> impl IntoResponse {
    Json(host.shared())
}

/// The thread's snapshot on the first line, then its run's events, if one is
/// in progress.
async fn open(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> Reply<Response> {
    let opened = host.open(&id)?;
    let snapshot = stream::once(async move { opened.snapshot });
    let lines = ndjson(snapshot).chain(ndjson(
        opened.events.unwrap_or_else(|| stream::empty().boxed()),
    ));
    Ok(ndjson_response(lines))
}

async fn start(
    State(host): State<Arc<Host>>,
    Path(id): Path<SessionId>,
    Json(request): Json<RunRequest>,
) -> Reply<Response> {
    let events = host.start(id, request).map_err(|error| match error {
        StartError::Busy => Failure(StatusCode::CONFLICT, error.to_string()),
        StartError::Refused(_) => Failure(StatusCode::BAD_REQUEST, error.to_string()),
    })?;
    Ok(ndjson_response(ndjson(events)))
}

async fn stop(State(host): State<Arc<Host>>, Path(id): Path<SessionId>) -> StatusCode {
    match host.stop(&id) {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
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
        Body::from_stream(lines),
    )
        .into_response()
}
