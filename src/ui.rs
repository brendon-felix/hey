/* -------------------------------------------------------------------------- */
/*                                   ui.rs                                    */
/* -------------------------------------------------------------------------- */

use std::io::{ErrorKind, IsTerminal};

use anyhow::{Result, bail};
use dialoguer::{FuzzySelect, Password, Select, theme::ColorfulTheme};

use crate::protocol::Decision;

/// Asks whether to run `tool`. Esc declines the call; Ctrl+C stops the run.
pub async fn confirm_tool(tool: String) -> Decision {
    let ask = move || {
        Select::with_theme(&ColorfulTheme::default())
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
    let secret = Password::with_theme(&ColorfulTheme::default())
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
    let picked = FuzzySelect::with_theme(&ColorfulTheme::default())
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
