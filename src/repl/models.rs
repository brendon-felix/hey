/* -------------------------------------------------------------------------- */
/*                               repl/models.rs                               */
/* -------------------------------------------------------------------------- */

use reedline::{Span, Suggestion};

use crate::backend::Backend;
use crate::protocol::{ModelChoice, Price};
use crate::render;

use super::mentions::{self, Index};

/// Widest a reference is padded to, so a long one does not push every
/// row's details off screen.
const REFERENCE_WIDTH: usize = 36;

/// Fetches the host's models into `index` for completion, without holding
/// up the prompt.
pub fn fetch(backend: Backend, index: Index) {
    tokio::spawn(async move {
        match backend.models().await {
            Ok(models) => mentions::lock(&index).models = models.models,
            Err(error) => tracing::warn!("no model completions: {error:#}"),
        }
    });
}

/// What sets `model` apart: its name, context window, thinking and price.
pub fn details(model: &ModelChoice) -> String {
    let window = model
        .context_window
        .map(|window| format!("{} context", render::tokens(u64::from(window))));
    let thinking = model.thinking.then(|| "thinking".to_owned());
    let price = model.price.map(price);
    model
        .name
        .clone()
        .into_iter()
        .chain(window)
        .chain(thinking)
        .chain(price)
        .collect::<Vec<_>>()
        .join(" · ")
}

fn price(price: Price) -> String {
    format!("${}/${} per M", dollars(price.input), dollars(price.output))
}

fn dollars(amount: f64) -> String {
    let cents = format!("{amount:.2}");
    cents.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A row per model for the picker: the current one marked, references in a
/// column, each row within the terminal.
pub fn rows(models: &[ModelChoice], current: &str) -> Vec<String> {
    let width = models
        .iter()
        .map(|model| model.reference.chars().count())
        .max()
        .unwrap_or_default()
        .min(REFERENCE_WIDTH);
    let room = render::terminal_width().saturating_sub(4);
    models
        .iter()
        .map(|model| {
            let mark = if model.reference == current {
                '✓'
            } else {
                ' '
            };
            let row = format!("{mark} {:<width$}  {}", model.reference, details(model));
            render::clip(row.trim_end(), room)
        })
        .collect()
}

/// The models whose reference or name holds `typed`, completing `span`.
pub fn suggestions(models: &[ModelChoice], typed: &str, span: Span) -> Vec<Suggestion> {
    let typed = typed.to_lowercase();
    models
        .iter()
        .filter(|model| {
            std::iter::once(&model.reference)
                .chain(&model.name)
                .any(|text| text.to_lowercase().contains(&typed))
        })
        .map(|model| Suggestion {
            value: model.reference.clone(),
            description: Some(details(model)).filter(|details| !details.is_empty()),
            span,
            ..Suggestion::default()
        })
        .collect()
}
