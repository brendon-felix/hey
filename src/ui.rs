/* -------------------------------------------------------------------------- */
/*                                   ui.rs                                    */
/* -------------------------------------------------------------------------- */

use anyhow::Result;
use dialoguer::{FuzzySelect, Select, theme::ColorfulTheme};

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

/// Picks one of `items`, starting at `current`; `None` when cancelled.
pub fn pick(prompt: &str, items: &[String], current: usize) -> Result<Option<usize>> {
    println!();
    let picked = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(items)
        .default(current)
        .max_length(15)
        .interact_opt()?;
    println!();
    Ok(picked)
}
