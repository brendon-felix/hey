/* -------------------------------------------------------------------------- */
/*                                 client.rs                                  */
/* -------------------------------------------------------------------------- */

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::stream::BoxStream;
use futures::{Stream, StreamExt, TryStreamExt, future, stream};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url, header, redirect};
use serde::de::DeserializeOwned;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::backend::{Events, Opened};
use crate::config::{self, Remote};
use crate::host::HEARTBEAT;
use crate::protocol::{
    Activities, AgentId, AgentInfo, AgentSnapshot, ApprovalId, Ask, CallId, CallResult, ClientInfo,
    Decision, Defaults, Event, FollowerId, Lent, Logged, MACHINE_HEADER, Machine, ModelInfo,
    Models, Revision, RunId, RunRequest, Snapshot, ThreadInfo, Tools, Undone,
};
use crate::retry::{Attempts, Backoff};
use crate::sessions::SessionId;
use crate::skills::Found;
use crate::steer::Steer;
use crate::subagents::Kinds;
use crate::{render, token, ui};

/// A host served by `hey --serve` on another machine.
#[derive(Clone)]
pub struct Client {
    name: String,
    base: Url,
    token: String,
    http: reqwest::Client,
    /// This machine and the host as it names it, told with every run.
    info: Arc<ClientInfo>,
}

impl Client {
    /// Connects with the token `remote` configures, else the one hey kept,
    /// else one the user enters, which hey keeps once the host accepts it.
    /// The host's agents can call the tools this machine `lent` while one of
    /// its runs is followed from here.
    pub async fn connect(
        name: &str,
        remote: &Remote,
        machine: Machine,
        lent: Lent,
    ) -> Result<Self> {
        let mut url = config::expand(&remote.url)?;
        if !url.ends_with('/') {
            url.push('/');
        }
        let base = Url::parse(&url).with_context(|| format!("invalid url for remote `{name}`"))?;
        let mut headers: header::HeaderMap = config::expand_headers(&remote.headers)?;
        let machine_name = header::HeaderValue::from_str(&lent.machine)
            .with_context(|| format!("machine name `{}` cannot be sent", lent.machine))?;
        headers.insert(MACHINE_HEADER, machine_name);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Probe idle connections, so a path that died quietly (sleep, a
            // changed network, a proxy dropping state) is found out in
            // seconds rather than when the next read fails mid-body.
            .tcp_keepalive(Duration::from_secs(20))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .default_headers(headers)
            // A host never redirects, so a redirect comes from a proxy's sign-in.
            .redirect(redirect::Policy::none())
            .build()?;
        let mut client = Self {
            name: name.to_owned(),
            token: String::new(),
            http,
            info: Arc::new(ClientInfo {
                machine,
                remote: name.to_owned(),
                url: base.to_string(),
                lent: Some(lent),
                skills: Vec::new(),
                project: std::env::current_dir()
                    .ok()
                    .and_then(|dir| crate::projects::detect(&dir)),
            }),
            base,
        };
        if let Some(token) = token::configured(&remote.token)? {
            client.token = token;
            if !client.accepted().await? {
                bail!(
                    "remote `{name}` refused the token from {}",
                    token::describe(&remote.token)
                );
            }
            return Ok(client);
        }
        let mut kept = token::kept(name)?;
        loop {
            let entered = kept.is_none();
            client.token = match kept.take() {
                Some(token) => token,
                None => client.ask()?,
            };
            if client.accepted().await? {
                if entered {
                    let place = token::keep(name, &client.token)?;
                    render::notice(&format!("kept the token for `{name}` in {place}"));
                }
                return Ok(client);
            }
            render::notice(&format!("remote `{name}` refused the token"));
        }
    }

    fn ask(&self) -> Result<String> {
        let name = &self.name;
        let why = format!(
            "no token for remote `{name}`: run hey in a terminal to enter it, \
             or set `token` or `token_file` in `[remotes.{name}]`"
        );
        ui::secret(
            &format!("Token for `{name}` (from the host's token file)"),
            &why,
        )?
        .with_context(|| format!("no token entered for remote `{name}`"))
    }

    /// Whether the host accepts the token.
    async fn accepted(&self) -> Result<bool> {
        let response = self.send(self.get("v1/defaults")?).await?;
        match refused_token(&response) {
            true => Ok(false),
            false => self.check(response).await.map(|_| true),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn defaults(&self) -> Result<Defaults> {
        self.json(self.get("v1/defaults")?).await
    }

    pub async fn model(&self, reference: &str) -> Result<ModelInfo> {
        self.json(self.get("v1/model")?.query(&[("reference", reference)]))
            .await
    }

    pub async fn models(&self) -> Result<Models> {
        self.json(self.get("v1/models")?).await
    }

    pub async fn threads(&self) -> Result<Vec<ThreadInfo>> {
        self.json(self.get("v1/threads")?).await
    }

    pub async fn viewed(&self, id: &SessionId) -> Result<()> {
        let path = format!("v1/threads/{}/viewed", id.as_str());
        self.ok(self.http.post(self.url(&path)?)).await.map(drop)
    }

    pub async fn archive(&self, id: &SessionId, archived: bool) -> Result<()> {
        let path = format!("v1/threads/{}/archived", id.as_str());
        self.ok(self.http.put(self.url(&path)?).json(&archived))
            .await
            .map(drop)
    }

    pub async fn fork(&self, id: &SessionId) -> Result<SessionId> {
        let path = format!("v1/threads/{}/fork", id.as_str());
        self.json(self.http.post(self.url(&path)?)).await
    }

    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        let path = format!("v1/threads/{}", id.as_str());
        self.ok(self.http.delete(self.url(&path)?)).await.map(drop)
    }

    pub async fn activity(&self) -> Result<BoxStream<'static, Result<Activities>>> {
        let response = self.ok(self.get("v1/activity")?).await?;
        Ok(parse(lines(response)))
    }

    pub async fn shared(&self) -> Result<Vec<String>> {
        self.json(self.get("v1/shared")?).await
    }

    pub async fn skills(&self) -> Result<Found> {
        self.json(self.get("v1/skills")?).await
    }

    pub async fn agents(&self) -> Result<Kinds> {
        self.json(self.get("v1/agents")?).await
    }

    pub async fn prompts(&self) -> Result<Vec<crate::mcp::Prompt>> {
        self.json(self.get("v1/prompts")?).await
    }

    pub async fn open(&self, id: &SessionId) -> Result<Opened> {
        self.opened(id, Role::Follower).await
    }

    /// The thread as [`Self::open`] gives it, but naming no machine, so the
    /// host hands this view none of its run's tool calls.
    pub async fn view(&self, id: &SessionId) -> Result<Opened> {
        self.opened(id, Role::Viewer).await
    }

    async fn opened(&self, id: &SessionId, role: Role) -> Result<Opened> {
        let request = role.mark(self.get(&format!("v1/threads/{}", id.as_str()))?);
        let mut lines = lines(self.ok(request).await?);
        let first = lines.next().await.context("the host sent no thread")??;
        let snapshot: Snapshot =
            serde_json::from_str(&first).context("the host sent an unreadable thread")?;
        let events = snapshot
            .running
            .is_some()
            .then(|| self.follow(id, lines.boxed(), role));
        Ok(Opened { snapshot, events })
    }

    pub async fn subagents(&self, id: &SessionId) -> Result<Vec<AgentInfo>> {
        self.json(self.get(&format!("v1/threads/{}/agents", id.as_str()))?)
            .await
    }

    /// `None` when the host knows no such subagent.
    pub async fn subagent(&self, id: &SessionId, agent: &AgentId) -> Result<Option<AgentSnapshot>> {
        let path = format!("v1/threads/{}/agents/{}", id.as_str(), agent.as_str());
        let response = self.send(self.get(&path)?).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = self.check(response).await?;
        let snapshot = response
            .json()
            .await
            .with_context(|| format!("remote `{}` sent an unreadable answer", self.name))?;
        Ok(Some(snapshot))
    }

    pub async fn start(
        &self,
        id: &SessionId,
        request: RunRequest,
        lent: Option<Lent>,
    ) -> Result<Events> {
        let mut info = ClientInfo::clone(&self.info);
        if lent.is_some() {
            info.lent = lent;
        }
        info.skills = crate::skills::find(&crate::agent::working_directory()).skills;
        info.project = crate::projects::detect(&crate::agent::working_directory());
        let request = RunRequest {
            client: Some(info),
            ..request
        };
        let path = format!("v1/threads/{}/run", id.as_str());
        let response = self
            .ok(self.http.post(self.url(&path)?).json(&request))
            .await?;
        Ok(self.follow(id, lines(response).boxed(), Role::Follower))
    }

    /// The events of a run of thread `id` on `lines`, followed again when
    /// they break off.
    fn follow(&self, id: &SessionId, lines: Lines, role: Role) -> Events {
        let resumable = Resumable {
            client: self.clone(),
            thread: id.clone(),
            role,
            link: Link::Open(lines),
            position: None,
        };
        resumable.events()
    }

    /// The lines of run `run` of thread `id` after its first `after`;
    /// `None` once the host no longer has the run. Sent once: following a
    /// run again retries by itself.
    async fn resume(
        &self,
        id: &SessionId,
        run: RunId,
        after: usize,
        role: Role,
    ) -> Result<Option<Lines>> {
        let path = format!("v1/threads/{}/runs/{}", id.as_str(), run.0);
        let request = role.mark(self.get(&path)?.query(&[("after", after)]));
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("cannot reach remote `{}`", self.name))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(lines(self.check(response).await?).boxed()))
    }

    pub async fn answer(
        &self,
        id: &SessionId,
        approval: ApprovalId,
        decision: Decision,
    ) -> Result<bool> {
        let path = format!("v1/threads/{}/approvals/{approval}", id.as_str());
        self.done(
            self.http.post(self.url(&path)?).json(&decision),
            StatusCode::CONFLICT,
        )
        .await
    }

    pub async fn stop(&self, id: &SessionId) -> Result<bool> {
        let path = format!("v1/threads/{}/run", id.as_str());
        self.done(self.http.delete(self.url(&path)?), StatusCode::NOT_FOUND)
            .await
    }

    pub async fn steer(&self, id: &SessionId, steer: &Steer) -> Result<bool> {
        let path = format!("v1/threads/{}/steer", id.as_str());
        self.done(
            self.http.post(self.url(&path)?).json(steer),
            StatusCode::CONFLICT,
        )
        .await
    }

    pub async fn unsteer(&self, id: &SessionId, prompt: &str) -> Result<bool> {
        let path = format!("v1/threads/{}/steer", id.as_str());
        self.done(
            self.http.delete(self.url(&path)?).json(prompt),
            StatusCode::CONFLICT,
        )
        .await
    }

    pub async fn compact(&self, id: &SessionId, focus: &str) -> Result<bool> {
        let path = format!("v1/threads/{}/compact", id.as_str());
        self.done(
            self.http.post(self.url(&path)?).json(focus),
            StatusCode::CONFLICT,
        )
        .await
    }

    pub async fn undo(&self, id: &SessionId) -> Result<Option<Undone>> {
        let path = format!("v1/threads/{}/undo", id.as_str());
        self.json(self.http.post(self.url(&path)?)).await
    }

    pub async fn set_plan(&self, id: &SessionId, on: bool) -> Result<()> {
        let path = format!("v1/threads/{}/plan", id.as_str());
        self.ok(self.http.put(self.url(&path)?).json(&on))
            .await
            .map(drop)
    }

    pub async fn tools(&self, id: &SessionId) -> Result<Tools> {
        self.json(self.get(&format!("v1/threads/{}/tools", id.as_str()))?)
            .await
    }

    /// Takes up call `call` for `follower`, this terminal's view of the
    /// run; false when another terminal took it first, or it is no longer
    /// waited for.
    pub async fn claim(&self, id: &SessionId, call: CallId, follower: FollowerId) -> Result<bool> {
        let path = format!("{}/claim", call_path(id, call));
        self.done(
            self.http.post(self.url(&path)?).json(&follower),
            StatusCode::CONFLICT,
        )
        .await
    }

    /// Shows `event` of the call to every client following the run.
    pub async fn call_event(&self, id: &SessionId, call: CallId, event: &Event) -> Result<()> {
        let path = format!("{}/events", call_path(id, call));
        self.ok(self.http.post(self.url(&path)?).json(event))
            .await
            .map(drop)
    }

    /// Waits for a client following the run to decide `ask` for the call.
    /// The host keeps the response open with blank lines until then.
    pub async fn call_ask(&self, id: &SessionId, call: CallId, ask: &Ask) -> Result<Decision> {
        let path = format!("{}/asks", call_path(id, call));
        let response = self.ok(self.http.post(self.url(&path)?).json(ask)).await?;
        let mut decisions = parse::<Option<Decision>>(lines(response));
        decisions
            .next()
            .await
            .context("the host sent no decision")??
            .context("the call is no longer waited for")
    }

    /// Reports how the call went; false when it was no longer waited for.
    pub async fn finish(&self, id: &SessionId, call: CallId, result: &CallResult) -> Result<bool> {
        let path = call_path(id, call);
        self.done(
            self.http.post(self.url(&path)?).json(result),
            StatusCode::NOT_FOUND,
        )
        .await
    }

    /// What the host holds of the synced path `name` from this machine.
    pub async fn sync_manifest(&self, name: &str) -> Result<crate::sync::Manifest> {
        self.json(self.get(&format!("v1/sync/{name}"))?).await
    }

    pub async fn sync_put(&self, name: &str, path: &str, bytes: Vec<u8>) -> Result<()> {
        let request = self
            .http
            .put(self.url(&format!("v1/sync/{name}/file"))?)
            .query(&[("path", path)])
            .body(bytes);
        self.ok(request).await.map(drop)
    }

    pub async fn sync_remove(&self, name: &str, path: &str) -> Result<()> {
        let request = self
            .http
            .delete(self.url(&format!("v1/sync/{name}/file"))?)
            .query(&[("path", path)]);
        self.ok(request).await.map(drop)
    }

    pub async fn memories(&self) -> Result<Vec<String>> {
        self.json(self.get("v1/memories")?).await
    }

    pub async fn forget(&self, memory: &str) -> Result<bool> {
        self.done(
            self.http.delete(self.url("v1/memories")?).json(memory),
            StatusCode::NOT_FOUND,
        )
        .await
    }

    pub async fn remember(&self, memory: &str) -> Result<bool> {
        self.done(
            self.http.post(self.url("v1/memories")?).json(memory),
            StatusCode::CONFLICT,
        )
        .await
    }

    pub async fn revise(&self, revision: &Revision) -> Result<()> {
        self.ok(self.http.put(self.url("v1/memories")?).json(revision))
            .await
            .map(drop)
    }

    pub async fn projects(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::projects::Project>> {
        self.json(self.get("v1/projects")?).await
    }

    fn project_url(&self, name: &str) -> Result<Url> {
        let mut url = self.url("v1/projects")?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("the host's address cannot hold a path"))?
            .push(name);
        Ok(url)
    }

    pub async fn change_project(&self, name: &str, change: &crate::projects::Change) -> Result<bool> {
        self.done(
            self.http.put(self.project_url(name)?).json(change),
            StatusCode::NOT_FOUND,
        )
        .await
    }

    pub async fn remove_project(&self, name: &str) -> Result<bool> {
        self.done(self.http.delete(self.project_url(name)?), StatusCode::NOT_FOUND)
            .await
    }

    fn url(&self, path: &str) -> Result<Url> {
        Ok(self.base.join(path)?)
    }

    fn get(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.get(self.url(path)?))
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        let response = self.ok(request).await?;
        response
            .json()
            .await
            .with_context(|| format!("remote `{}` sent an unreadable answer", self.name))
    }

    /// True when the host did it; false when it answered `not_applicable`.
    async fn done(&self, request: RequestBuilder, not_applicable: StatusCode) -> Result<bool> {
        let response = self.send(request).await?;
        match response.status() {
            status if status == not_applicable => Ok(false),
            _ => self.check(response).await.map(|_| true),
        }
    }

    async fn ok(&self, request: RequestBuilder) -> Result<Response> {
        let response = self.send(request).await?;
        self.check(response).await
    }

    /// Sends `request`, again while it fails without reaching the host.
    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let request = request.bearer_auth(&self.token).build()?;
        let mut attempts = Attempts::new(Backoff::REQUEST);
        loop {
            let copy = request
                .try_clone()
                .context("a request with a streamed body cannot be sent again")?;
            let sent = self.http.execute(copy).await;
            let missed = match &sent {
                Ok(response) => unreached(request.method(), response.status()),
                Err(error) => error.is_connect() || error.is_timeout() || stale(error),
            };
            let Some(retry) = missed.then(|| attempts.next(None)).flatten() else {
                return sent.with_context(|| format!("cannot reach remote `{}`", self.name));
            };
            tracing::warn!(remote = %self.name, url = %request.url(), %retry, "a request did not reach the host");
            tokio::time::sleep(retry.delay).await;
        }
    }

    async fn check(&self, response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let proxied = status == StatusCode::UNAUTHORIZED && !refused_token(&response);
        if status.is_redirection() || proxied {
            bail!(
                "remote `{0}` answered {status}, as a sign-in proxy in front of it does; \
                 give its credentials in `headers` in `[remotes.{0}]`",
                self.name
            );
        }
        let reason = response.text().await.unwrap_or_default();
        match reason.trim() {
            "" => bail!("remote `{}` answered {status}", self.name),
            reason => bail!("remote `{}`: {reason}", self.name),
        }
    }
}

fn call_path(id: &SessionId, call: CallId) -> String {
    format!("v1/threads/{}/calls/{call}", id.as_str())
}

/// The host's own refusal, told apart by its challenge from a proxy's in front of it.
fn refused_token(response: &Response) -> bool {
    response.status() == StatusCode::UNAUTHORIZED
        && response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .is_some_and(|challenge| challenge == "Bearer")
}

/// Whether a proxy answered for the host it could not reach, so the host
/// never saw the request. A gateway timeout leaves that open, so only a
/// request that changes nothing is sent again after one.
fn unreached(method: &Method, status: StatusCode) -> bool {
    match status {
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => true,
        StatusCode::GATEWAY_TIMEOUT => method == Method::GET,
        _ => false,
    }
}

/// The stream's lines, without the blank ones the host sends to keep it open.
fn lines(response: Response) -> impl Stream<Item = Result<String>> + Send + Unpin + 'static {
    let bytes = response.bytes_stream().map_err(io::Error::other);
    FramedRead::new(StreamReader::new(bytes), LinesCodec::new())
        .map(|line| line.map_err(|error| anyhow!("lost the connection to the host: {error:#?}")))
        .try_filter(|line| future::ready(!line.is_empty()))
}

type Lines = BoxStream<'static, Result<String>>;

/// How long a run's stream may stay silent before it counts as lost: the
/// host says something at least every heartbeat.
const SILENCE: Duration = HEARTBEAT.saturating_mul(2);

/// How a terminal looks at a run: a follower runs the run's tool calls for
/// this machine, a viewer does not.
#[derive(Debug, Clone, Copy)]
enum Role {
    Follower,
    Viewer,
}

impl Role {
    fn mark(self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Role::Follower => request,
            Role::Viewer => request.header(MACHINE_HEADER, ""),
        }
    }
}

/// A run's events as the host streams them, followed again from the last
/// that arrived when the stream breaks or falls silent before the run ends.
struct Resumable {
    client: Client,
    thread: SessionId,
    role: Role,
    link: Link,
    /// The run, and how many of its events arrived, once the host said.
    position: Option<Position>,
}

enum Link {
    Open(Lines),
    /// The stream broke this way; following the run again comes next.
    Broken(anyhow::Error),
    Ended,
}

#[derive(Debug, Clone, Copy)]
struct Position {
    run: RunId,
    at: usize,
}

impl Resumable {
    fn events(self) -> Events {
        stream::unfold(self, Self::next).boxed()
    }

    async fn next(mut self) -> Option<(Result<Event>, Self)> {
        loop {
            match std::mem::replace(&mut self.link, Link::Ended) {
                Link::Ended => return None,
                Link::Open(mut lines) => {
                    let line = match tokio::time::timeout(SILENCE, lines.next()).await {
                        Ok(Some(Ok(line))) => line,
                        Ok(Some(Err(error))) => {
                            if let Some(error) = self.broke(error) {
                                return Some((Err(error), self));
                            }
                            continue;
                        }
                        Ok(None) => {
                            let error = anyhow!("the host ended the stream before the run ended");
                            if let Some(error) = self.broke(error) {
                                return Some((Err(error), self));
                            }
                            continue;
                        }
                        Err(_) => {
                            let error = anyhow!("the host said nothing for {}s", SILENCE.as_secs());
                            if let Some(error) = self.broke(error) {
                                return Some((Err(error), self));
                            }
                            continue;
                        }
                    };
                    let logged = match serde_json::from_str::<Logged>(&line) {
                        Ok(logged) => logged,
                        Err(error) => {
                            return Some((
                                Err(error).context("the host sent something unreadable"),
                                self,
                            ));
                        }
                    };
                    self.arrived(&logged);
                    if !matches!(logged.event, Event::End { .. }) {
                        self.link = Link::Open(lines);
                    }
                    return Some((Ok(logged.event), self));
                }
                Link::Broken(lost) => {
                    match reconnect(&self.client, &self.thread, self.role, self.position, lost)
                        .await
                    {
                        Ok(lines) => self.link = Link::Open(lines),
                        Err(error) => return Some((Err(error), self)),
                    }
                }
            }
        }
    }

    fn arrived(&mut self, logged: &Logged) {
        let Some(at) = logged.at else {
            return;
        };
        self.position = match (&logged.event, self.position) {
            (Event::Began { run, .. }, _) => Some(Position { run: *run, at }),
            (_, position) => position.map(|position| Position { at, ..position }),
        };
    }

    /// Tells of the stream breaking with `error`, before following the run
    /// again; fails at once when the host gave no position to resume from.
    /// Resuming is routine (proxies and tunnels reset idle streams), so it is
    /// only logged, not shown; returns the error when it cannot be resumed.
    fn broke(&mut self, error: anyhow::Error) -> Option<anyhow::Error> {
        if self.position.is_none() {
            return Some(error);
        }
        tracing::warn!(
            thread = self.thread.as_str(),
            error = format!("{error:#}"),
            "lost a run's stream"
        );
        self.link = Link::Broken(error);
        None
    }
}

/// The stream of a run of `thread` from `position`, where it broke with
/// `lost`, asked for again while the host cannot be reached.
async fn reconnect(
    client: &Client,
    thread: &SessionId,
    role: Role,
    position: Option<Position>,
    lost: anyhow::Error,
) -> Result<Lines> {
    let Some(Position { run, at }) = position else {
        return Err(lost);
    };
    let mut attempts = Attempts::new(Backoff::RECONNECT);
    loop {
        let failure = match client.resume(thread, run, at, role).await {
            Ok(Some(lines)) => {
                tracing::info!(
                    thread = thread.as_str(),
                    ?run,
                    at,
                    "following the run again"
                );
                return Ok(lines);
            }
            Ok(None) => {
                bail!("{lost:#}; the run is no longer on the host, so `/view` shows what it said")
            }
            Err(failure) => failure,
        };
        let Some(retry) = attempts.next(None) else {
            return Err(failure.context(format!("{lost:#}; following the run again failed")));
        };
        tracing::warn!(thread = thread.as_str(), error = format!("{failure:#}"), %retry, "cannot follow the run again");
        tokio::time::sleep(retry.delay).await;
    }
}

/// Each line of `lines` as a `T`.
fn parse<T: DeserializeOwned + Send + 'static>(
    lines: impl Stream<Item = Result<String>> + Send + 'static,
) -> BoxStream<'static, Result<T>> {
    lines
        .map(|line| serde_json::from_str::<T>(&line?).context("the host sent something unreadable"))
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;

    use axum::body::{Body, Bytes};
    use axum::extract::{Path, Query};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use futures::stream;

    use super::*;
    use crate::protocol::{Ending, RunOptions};

    const RUN: RunId = RunId(9);

    fn line(at: usize, event: Event) -> Result<Bytes, std::io::Error> {
        let logged = Logged {
            at: Some(at),
            event,
        };
        Ok(Bytes::from(
            serde_json::to_string(&logged).unwrap_or_default() + "\n",
        ))
    }

    fn began(at: usize) -> Result<Bytes, std::io::Error> {
        let began = Event::Began {
            run: RUN,
            elapsed: Duration::ZERO,
            follower: None,
        };
        line(at, began)
    }

    #[tokio::test]
    async fn a_run_whose_stream_breaks_is_followed_again_from_the_last_event() {
        let cut = || async {
            let lines = stream::iter([began(0), line(1, Event::Text { text: "a".into() })]);
            let lost = stream::once(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err(std::io::Error::other("connection reset"))
            });
            Body::from_stream(lines.chain(lost))
        };
        let again = |Path((_, run)): Path<(String, u64)>,
                     Query(after): Query<HashMap<String, usize>>| async move {
            assert_eq!(
                (run, after.get("after")),
                (RUN.0, Some(&1)),
                "followed again after what arrived"
            );
            let end = Event::End {
                ending: Ending::Answered,
                saved: 2,
            };
            let lines = [
                began(1),
                line(2, Event::Text { text: "b".into() }),
                line(3, end),
            ];
            Ok::<_, Infallible>(Body::from_stream(stream::iter(lines)))
        };
        let app = Router::new()
            .route("/v1/threads/{id}/run", post(cut))
            .route("/v1/threads/{id}/runs/{run}", get(again))
            .route("/v1/defaults", get(|| async { Json(()) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = Client {
            name: "test".into(),
            base: base.clone(),
            token: String::new(),
            http: reqwest::Client::new(),
            info: Arc::new(ClientInfo {
                machine: Machine::here(None),
                remote: "test".into(),
                url: base.to_string(),
                lent: None,
                skills: Vec::new(),
                project: None,
            }),
        };
        let request = RunRequest {
            prompt: "hi".into(),
            attachments: Vec::new(),
            options: RunOptions {
                model: "test/model".into(),
                thinking: None,
                system_prompt: None,
            },
            client: None,
            task: Default::default(),
            goal: None,
            surface: Default::default(),
        };

        let events: Vec<Event> = client
            .start(&SessionId::new(), request, None)
            .await
            .unwrap()
            .map(|event| event.unwrap())
            .collect()
            .await;
        let told: Vec<String> = events
            .iter()
            .map(|event| match event {
                Event::Began { .. } => "began".into(),
                Event::Text { text } => text.clone(),
                Event::Notice { text } => format!("notice: {text}"),
                Event::End { .. } => "end".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(told, ["began", "a", "began", "b", "end"]);
    }
}

/// Whether `error` is a pooled connection found dead as the request was
/// written to it (a proxy or tunnel dropped it while idle). The request is
/// sent again on a new connection.
fn stale(error: &reqwest::Error) -> bool {
    use std::error::Error as _;
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
            )
        {
            return true;
        }
        let text = cause.to_string();
        if text.contains("connection reset")
            || text.contains("connection closed before message completed")
            || text.contains("connection was not ready")
        {
            return true;
        }
        source = cause.source();
    }
    false
}
