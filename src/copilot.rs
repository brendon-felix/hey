/* -------------------------------------------------------------------------- */
/*                                 copilot.rs                                 */
/* -------------------------------------------------------------------------- */

//! GitHub Copilot as a vendor. Its API takes a session token that lasts
//! about half an hour, exchanged for a GitHub token, so models are connected
//! with a client that signs every request with the current session.

use std::future::Future;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use http::header::{AUTHORIZATION, HeaderValue};
use http::{Request, Uri};
use rig_core::Model;
use rig_core::http_client::{
    self, Bytes, DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Response, StreamingResponse,
};
use rig_core::model::ModelInfo;
use rig_core::providers::copilot::DIALECT;
use rig_core::providers::copilot::auth::{
    AuthContext, AuthSource, Authenticator, DeviceCodeHandler, FileTokenStore, TokenStore,
};
use rig_core::providers::copilot::model_listing::Models;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::providers::registry::ConnectOptions;
use rig_core::wasm_compat::WasmCompatSend;

use crate::config;

pub const VENDOR: &str = rig_core::providers::copilot::PROVIDER_NAME;

/// Where the Copilot dialect sends requests when no session names its own
/// endpoint; sessions name one per plan, such as
/// `api.individual.githubcopilot.com`.
const DEFAULT_HOST: &str = "api.githubcopilot.com";

/// Stands in for the session token while a model is connected; every
/// request is signed with the session current when it is sent.
const UNSIGNED: &str = "signed-per-request";

fn store() -> Result<FileTokenStore> {
    Ok(FileTokenStore::new(config::data_dir()?.join("auth")))
}

fn record() -> Result<PathBuf> {
    Ok(store()?.path(VENDOR))
}

pub fn signed_in() -> bool {
    record().is_ok_and(|path| path.exists())
}

/// Signs in to GitHub with a device code, replacing any earlier sign-in.
pub async fn login() -> Result<()> {
    let store = store()?;
    store
        .delete(VENDOR)
        .await
        .map_err(|error| anyhow::anyhow!("cannot remove the earlier sign-in: {error}"))?;
    let prompt = DeviceCodeHandler::new(|prompt| {
        println!(
            "Open {} and enter the code {}",
            prompt.verification_uri, prompt.user_code
        );
    });
    Authenticator::new(AuthSource::OAuth, store, prompt, true)
        .auth_context(&rig_reqwest::shared())
        .await
        .context("cannot sign in to GitHub Copilot")?;
    println!("Signed in to GitHub Copilot.");
    Ok(())
}

/// `options` signing each request with the session of the saved sign-in.
pub fn signed(options: ConnectOptions) -> Result<ConnectOptions> {
    Ok(options.api_key(UNSIGNED).http(session()?))
}

/// The chat models the signed-in plan offers, at `base_url` when one is
/// configured.
pub async fn models(base_url: Option<String>) -> Result<Vec<ModelInfo>> {
    let config = OpenAIConfig::with_key(&DIALECT, UNSIGNED);
    let config = match base_url {
        Some(url) => config.with_base_url(url),
        None => config,
    };
    let models = Model::new(Models::new(config), session()?).list().await?;
    Ok(models
        .into_iter()
        .filter(|model| model.r#type.as_deref().is_none_or(|kind| kind == "chat"))
        .collect())
}

fn session() -> Result<Session> {
    if !signed_in() {
        bail!("not signed in to GitHub Copilot: run `hey --login copilot`");
    }
    Ok(Session {
        http: rig_reqwest::shared(),
        auth: Authenticator::new(
            AuthSource::OAuth,
            store()?,
            DeviceCodeHandler::default(),
            false,
        ),
    })
}

/// Sends requests through `http`, each with the current Copilot session.
#[derive(Clone)]
struct Session {
    http: DynHttpClient,
    auth: Authenticator,
}

impl Session {
    // The error is rig's transport error, which every send returns anyway.
    #[allow(clippy::result_large_err)]
    async fn sign<T>(&self, mut request: Request<T>) -> http_client::Result<Request<T>> {
        let context = self.auth.auth_context(&self.http).await.map_err(|error| {
            http_client::Error::instance(std::io::Error::other(format!(
                "GitHub Copilot gave no session ({error}); run `hey --login copilot`"
            )))
        })?;
        let token = HeaderValue::from_str(&format!("Bearer {}", context.api_key.expose()))
            .map_err(http_client::Error::instance)?;
        request.headers_mut().insert(AUTHORIZATION, token);
        if let Some(uri) = rebased(request.uri(), &context) {
            *request.uri_mut() = uri.map_err(http_client::Error::instance)?;
        }
        Ok(request)
    }
}

/// `uri` at the session's endpoint, unless it was sent to another host than
/// Copilot's default, such as a configured `base_url`.
fn rebased(uri: &Uri, context: &AuthContext) -> Option<Result<Uri, http::uri::InvalidUri>> {
    let base = context.api_base.as_deref()?;
    (uri.host() == Some(DEFAULT_HOST)).then(|| {
        let path = uri.path_and_query().map_or("", |path| path.as_str());
        format!("{}{path}", base.trim_end_matches('/')).parse()
    })
}

impl HttpClientExt for Session {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let session = self.clone();
        let request = request.map(Into::<Bytes>::into);
        async move {
            let request = session.sign(request).await?;
            session.http.send(request).await
        }
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let session = self.clone();
        async move {
            let request = session.sign(request).await?;
            session.http.send_multipart(request).await
        }
    }

    fn send_streaming<T>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let request = request.map(Into::<Bytes>::into);
        async move {
            let request = self.sign(request).await?;
            self.http.send_streaming(request).await
        }
    }
}
