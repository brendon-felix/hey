/* -------------------------------------------------------------------------- */
/*                                  models.rs                                 */
/* -------------------------------------------------------------------------- */

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use rig_core::catalog::{Catalog, ModelSpec};
use rig_core::model::ModelInfo as Listed;
use rig_core::providers::registry::ProviderId;

use crate::agent;
use crate::config::{self, Providers};
use crate::copilot;
use crate::protocol::{ModelChoice, Models, Price};

const OLLAMA: &str = "ollama";

/// Vendors whose models depend on the setup, so the vendor lists them
/// rather than rig's catalog: what Ollama has pulled, and what the Copilot
/// plan offers.
const LISTED_LIVE: [&str; 2] = [OLLAMA, copilot::VENDOR];

/// How long a vendor may take to list its models.
const PATIENCE: Duration = Duration::from_secs(5);

/// Every model hey can use, by vendor: the catalog's for each vendor with an
/// API key set, Ollama's when a daemon answers, and Copilot's once signed in.
pub async fn list(providers: &Providers) -> Models {
    let (ollama, copilot) = tokio::join!(live(OLLAMA, pulled(providers)), async {
        match copilot::signed_in() {
            true => Some(live(copilot::VENDOR, offered(providers)).await),
            false => None,
        }
    });
    let mut problems = Vec::new();
    // No daemon is the usual reason Ollama does not answer, unless one is
    // configured.
    let ollama = ollama.unwrap_or_else(|problem| {
        if providers.contains_key(OLLAMA) {
            problems.push(problem);
        }
        Vec::new()
    });
    let copilot = match copilot {
        None => Vec::new(),
        Some(Ok(models)) => models,
        Some(Err(problem)) => {
            problems.push(problem);
            catalogued(|vendor| vendor == copilot::VENDOR).collect()
        }
    };
    let mut models: Vec<ModelChoice> =
        catalogued(|vendor| !LISTED_LIVE.contains(&vendor) && has_key(vendor))
            .chain(ollama)
            .chain(copilot)
            .collect();
    models.sort_by(|a, b| vendor(&a.reference).cmp(vendor(&b.reference)));
    Models { models, problems }
}

async fn live(
    vendor: &str,
    models: impl Future<Output = Result<Vec<ModelChoice>>>,
) -> Result<Vec<ModelChoice>, String> {
    match tokio::time::timeout(PATIENCE, models).await {
        Ok(Ok(models)) => Ok(models),
        Ok(Err(error)) => Err(format!("{vendor}: {error:#}")),
        Err(_) => Err(format!("{vendor}: no answer in {}s", PATIENCE.as_secs())),
    }
}

async fn pulled(providers: &Providers) -> Result<Vec<ModelChoice>> {
    let models = agent::ollama_daemon(providers)?
        .client()
        .list_models()
        .await?;
    Ok(models
        .into_iter()
        // Ollama does not say which models only embed; their names do.
        .filter(|model| !model.id.contains("embed"))
        .filter_map(|model| listed(OLLAMA, model))
        .collect())
}

async fn offered(providers: &Providers) -> Result<Vec<ModelChoice>> {
    let models = copilot::models(config::base_url(providers, copilot::VENDOR)?).await?;
    Ok(models
        .into_iter()
        .filter_map(|model| listed(copilot::VENDOR, model))
        .collect())
}

/// The catalog's models that call tools, of the vendors `include` names.
fn catalogued(include: impl Fn(&str) -> bool) -> impl Iterator<Item = ModelChoice> {
    Catalog::builtin()
        .iter()
        .filter(move |spec| spec.tools && !spec.deprecated && include(spec.provider.vendor()))
        .map(|spec| from_spec(spec.provider.vendor(), &spec.id, spec))
}

/// A model `vendor` listed, with what the catalog knows of it; `None` when
/// the catalog knows it calls no tools, which every answer needs.
fn listed(vendor: &str, model: Listed) -> Option<ModelChoice> {
    let spec = ProviderId::catalog(vendor)
        .and_then(|provider| Catalog::builtin().get_exact(provider, &model.id));
    match spec {
        Some(spec) if !spec.tools => None,
        Some(spec) => Some(ModelChoice {
            name: model
                .name
                .and_then(|name| named(&model.id, name))
                .or_else(|| named(&model.id, spec.display_name.clone())),
            ..from_spec(vendor, &model.id, spec)
        }),
        None => Some(ModelChoice {
            reference: format!("{vendor}/{}", model.id),
            name: model.name.and_then(|name| named(&model.id, name)),
            context_window: model.context_length,
            thinking: false,
            price: None,
        }),
    }
}

fn from_spec(vendor: &str, id: &str, spec: &ModelSpec) -> ModelChoice {
    ModelChoice {
        reference: format!("{vendor}/{id}"),
        name: named(id, spec.display_name.clone()),
        context_window: spec.context_window,
        thinking: spec.reasoning.supported(),
        // Ollama runs models on the daemon and Copilot bills a subscription,
        // so the API's token prices are not theirs.
        price: spec
            .pricing
            .filter(|_| !LISTED_LIVE.contains(&vendor))
            .map(|pricing| Price {
                input: pricing.input,
                output: pricing.output,
            }),
    }
}

/// `name`, unless it only repeats the id.
fn named(id: &str, name: String) -> Option<String> {
    Some(name).filter(|name| !name.is_empty() && name != id)
}

fn has_key(vendor: &str) -> bool {
    ProviderId::catalog(vendor).is_some_and(|provider| {
        provider
            .api_key_envs()
            .iter()
            .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
    })
}

fn vendor(reference: &str) -> &str {
    reference.split('/').next().unwrap_or_default()
}
