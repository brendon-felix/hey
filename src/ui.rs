/* -------------------------------------------------------------------------- */
/*                                   ui.rs                                    */
/* -------------------------------------------------------------------------- */

use std::io::{ErrorKind, IsTerminal};

use anyhow::{Result, bail};
use dialoguer::console::{self, Style};
use dialoguer::{FuzzySelect, Password, Select, theme::ColorfulTheme};

use crate::protocol::Decision;
use crate::render::{self, palette};

/// dialoguer's colourful theme in the current theme's colours.
fn theme() -> ColorfulTheme {
    let palette = palette();
    let style = |color| tint(Style::new().for_stderr(), color);
    let mark = |text: &str, color| style(color).apply_to(text.to_owned());
    ColorfulTheme {
        defaults_style: style(palette.value),
        prompt_prefix: mark("?", palette.accent),
        prompt_suffix: mark("›", palette.muted),
        success_prefix: mark("✔", palette.input),
        success_suffix: mark("·", palette.muted),
        hint_style: style(palette.muted),
        values_style: style(palette.input),
        active_item_style: style(palette.name),
        active_item_prefix: mark("❯", palette.accent),
        picked_item_prefix: mark("❯", palette.accent),
        fuzzy_match_highlight_style: style(palette.name).bold(),
        ..ColorfulTheme::default()
    }
}

fn tint(style: Style, color: render::Color) -> Style {
    match color {
        render::Color::Default => style,
        render::Color::Ansi(index) => style.fg(console::Color::Color256(index)),
        render::Color::Rgb(r, g, b) => style.true_color(r, g, b),
    }
}

/// Asks whether to run `tool`. Esc declines the call; Ctrl+C stops the run.
pub async fn confirm_tool(tool: String) -> Decision {
    let ask = move || {
        Select::with_theme(&theme())
            .with_prompt(format!("Run `{tool}`?"))
            .items(["Yes", &format!("Always allow `{tool}` in this conversation"), "No"])
            .default(0)
            .report(false)
            .interact_opt()
    };
    match tokio::task::spawn_blocking(ask).await {
        Ok(Ok(Some(0))) => Decision::Once,
        Ok(Ok(Some(1))) => Decision::Always,
        Ok(Ok(_)) => Decision::Decline,
        _ => Decision::Stop,
    }
}

/// Asks for a secret without showing it; `None` when nothing is entered.
/// `why_needed` explains the failure when there is no terminal to ask in.
pub fn secret(prompt: &str, why_needed: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        bail!("{why_needed}");
    }
    let secret = Password::with_theme(&theme())
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()?;
    Ok(Some(secret.trim().to_owned()).filter(|secret| !secret.is_empty()))
}

/// Picks one of `items`, starting at `current`; `None` when cancelled with
/// Esc or Ctrl+C.
pub fn pick(prompt: &str, items: &[String], current: usize) -> Result<Option<usize>> {
    // On Ctrl+C dialoguer raises SIGINT, which ends hey unless something
    // listens for it; the error that follows then reports the cancel.
    #[cfg(unix)]
    let _listening = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    println!();
    let picked = FuzzySelect::with_theme(&theme())
        .with_prompt(prompt)
        .items(items)
        .default(current)
        .max_length(15)
        .interact_opt();
    println!();
    match picked {
        Err(dialoguer::Error::IO(error)) if error.kind() == ErrorKind::Interrupted => Ok(None),
        picked => Ok(picked?),
    }
}
