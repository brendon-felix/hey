/* -------------------------------------------------------------------------- */
/*                                 client.rs                                  */
/* -------------------------------------------------------------------------- */

use std::io;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{Stream, StreamExt, TryStreamExt};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::backend::{Events, Opened};
use crate::config::{self, Remote};
use crate::protocol::{
    ApprovalId, Decision, Defaults, Event, ModelInfo, RunRequest, Snapshot, Tools,
};
use crate::sessions::{SessionId, Summary};
use crate::{render, token, ui};

/// A host served by `hey --serve` on another machine.
pub struct Client {
    name: String,
    base: Url,
    token: String,
    http: reqwest::Client,
}

impl Client {
    /// Connects with the token `remote` configures, else the one hey kept,
    /// else one the user enters, which hey keeps once the host accepts it.
    pub async fn connect(name: &str, remote: &Remote) -> Result<Self> {
        let mut url = config::expand(&remote.url)?;
        if !url.ends_with('/') {
            url.push('/');
        }
        let base = Url::parse(&url).with_context(|| format!("invalid url for remote `{name}`"))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let mut client = Self {
            name: name.to_owned(),
            base,
            token: String::new(),
            http,
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
        match response.status() {
            StatusCode::UNAUTHORIZED => Ok(false),
            _ => self.check(response).await.map(|_| true),
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

    pub async fn threads(&self) -> Result<Vec<Summary>> {
        self.json(self.get("v1/threads")?).await
    }

    pub async fn shared(&self) -> Result<Vec<String>> {
        self.json(self.get("v1/shared")?).await
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

    pub async fn start(&self, id: &SessionId, request: &RunRequest) -> Result<Events> {
        let path = format!("v1/threads/{}/run", id.as_str());
        let response = self
            .ok(self.http.post(self.url(&path)?).json(request))
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

    pub async fn tools(&self, id: &SessionId) -> Result<Tools> {
        self.json(self.get(&format!("v1/threads/{}/tools", id.as_str()))?)
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
        let reason = response.text().await.unwrap_or_default();
        match reason.trim() {
            "" => bail!("remote `{}` answered {status}", self.name),
            reason => bail!("remote `{}`: {reason}", self.name),
        }
    }
}

fn lines(response: Response) -> impl Stream<Item = Result<String>> + Send + Unpin + 'static {
    let bytes = response.bytes_stream().map_err(io::Error::other);
    FramedRead::new(StreamReader::new(bytes), LinesCodec::new())
        .map(|line| line.context("lost the connection to the host"))
}

fn events(lines: impl Stream<Item = Result<String>> + Send + 'static) -> Events {
    lines
        .map(|line| {
            let line = line?;
            serde_json::from_str::<Event>(&line).context("the host sent an unreadable event")
        })
        .boxed()
}
