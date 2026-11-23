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
use rig_core::catalog::Catalog;
use rig_core::http_client::{
    self, Bytes, DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Response, StreamingResponse,
};
use rig_core::model::ModelInfo;
use rig_core::operation::Completion;
use rig_core::providers::copilot::DIALECT;
use rig_core::providers::copilot::auth::{
    AuthContext, AuthSource, Authenticator, DeviceCodeHandler, FileTokenStore, TokenStore,
};
use rig_core::providers::copilot::model_listing::Models;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::providers::openai::Route;
use rig_core::providers::openai::wire::OpenAiWire;
use rig_core::providers::registry::ConnectOptions;
use rig_core::providers::registry::ProviderId;
use rig_core::wasm_compat::WasmCompatSend;
use rig_core::{DynModel, Model};

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

/// A completion model of the signed-in plan. Copilot serves OpenAI's newer
/// models only on `/responses`, which rig's Copilot dialect picks for `codex`
/// models alone, so the catalog's API for the model decides here.
pub fn connect(id: &str, base_url: Option<String>) -> Result<DynModel<Completion>> {
    let responses = ProviderId::catalog(VENDOR)
        .and_then(|provider| Catalog::builtin().get_exact(provider, id))
        .and_then(|spec| spec.details.provider.as_ref())
        .is_some_and(|api| {
            api.shape.as_deref() == Some("responses")
                || (api.shape.is_none() && api.npm.as_deref() == Some("@ai-sdk/openai"))
        })
        || id.to_ascii_lowercase().contains("codex");
    let mut config = OpenAIConfig::with_key(&DIALECT, UNSIGNED).with_route(match responses {
        true => Route::Responses,
        false => Route::Chat,
    });
    if let Some(url) = base_url {
        config = config.with_base_url(url);
    }
    Ok(Model::new(OpenAiWire::new(config, id), session()?).into())
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

/// Chat requests carry system messages as arrays of text parts, which
/// Copilot's Claude models ignore; sends them as plain strings.
fn flatten_system(request: &mut Request<Bytes>) {
    if !request.uri().path().ends_with("/chat/completions") {
        return;
    }
    let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(request.body()) else {
        return;
    };
    let mut changed = false;
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for message in messages {
            if message["role"] != "system" {
                continue;
            }
            let Some(parts) = message["content"].as_array() else {
                continue;
            };
            let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            message["content"] = text.join("\n\n").into();
            changed = true;
        }
    }
    if changed && let Ok(bytes) = serde_json::to_vec(&body) {
        *request.body_mut() = Bytes::from(bytes);
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
            let mut request = request;
            flatten_system(&mut request);
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
            let mut request = request;
            flatten_system(&mut request);
            let request = self.sign(request).await?;
            self.http.send_streaming(request).await
        }
    }
}
