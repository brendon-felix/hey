/* -------------------------------------------------------------------------- */
/*                                 client.rs                                  */
/* -------------------------------------------------------------------------- */

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::stream::BoxStream;
use futures::{Stream, StreamExt, TryStreamExt, future};
use reqwest::{RequestBuilder, Response, StatusCode, Url, header, redirect};
use serde::de::DeserializeOwned;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::backend::{Events, Opened};
use crate::config::{self, Remote};
use crate::protocol::{
    Activities, ApprovalId, Ask, CallId, CallResult, ClientInfo, Decision, Defaults, Event,
    FollowerId, Lent, MACHINE_HEADER, Machine, ModelInfo, Revision, RunRequest, Snapshot,
    ThreadInfo, Tools,
};
use crate::sessions::SessionId;
use crate::skills::Found;
use crate::steer::Steer;
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
    pub async fn connect(name: &str, remote: &Remote, machine: Machine, lent: Lent) -> Result<Self> {
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
        ui::secret(&format!("Token for `{name}` (from the host's token file)"), &why)?
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

    pub async fn models(&self) -> Result<Vec<String>> {
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

    pub async fn open(&self, id: &SessionId) -> Result<Opened> {
        let response = self
            .ok(self.get(&format!("v1/threads/{}", id.as_str()))?)
            .await?;
        let mut lines = lines(response);
        let first = lines.next().await.context("the host sent no thread")??;
        let snapshot: Snapshot =
            serde_json::from_str(&first).context("the host sent an unreadable thread")?;
        let events = snapshot.running.is_some().then(|| events(lines));
        Ok(Opened { snapshot, events })
    }

    pub async fn start(&self, id: &SessionId, request: RunRequest) -> Result<Events> {
        let request = RunRequest {
            client: Some(ClientInfo::clone(&self.info)),
            ..request
        };
        let path = format!("v1/threads/{}/run", id.as_str());
        let response = self
            .ok(self.http.post(self.url(&path)?).json(&request))
            .await?;
        Ok(events(lines(response)))
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

    pub async fn compact(&self, id: &SessionId, focus: &str) -> Result<bool> {
        let path = format!("v1/threads/{}/compact", id.as_str());
        self.done(
            self.http.post(self.url(&path)?).json(focus),
            StatusCode::CONFLICT,
        )
        .await
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

    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        request
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("cannot reach remote `{}`", self.name))
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

/// The stream's lines, without the blank ones the host sends to keep it open.
fn lines(response: Response) -> impl Stream<Item = Result<String>> + Send + Unpin + 'static {
    let bytes = response.bytes_stream().map_err(io::Error::other);
    FramedRead::new(StreamReader::new(bytes), LinesCodec::new())
        .map(|line| line.context("lost the connection to the host"))
        .try_filter(|line| future::ready(!line.is_empty()))
}

fn events(lines: impl Stream<Item = Result<String>> + Send + 'static) -> Events {
    parse(lines)
}

/// Each line of `lines` as a `T`.
fn parse<T: DeserializeOwned + Send + 'static>(
    lines: impl Stream<Item = Result<String>> + Send + 'static,
) -> BoxStream<'static, Result<T>> {
    lines
        .map(|line| serde_json::from_str::<T>(&line?).context("the host sent something unreadable"))
        .boxed()
}
