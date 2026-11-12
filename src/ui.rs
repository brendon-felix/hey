/* -------------------------------------------------------------------------- */
/*                                   ui.rs                                    */
/* -------------------------------------------------------------------------- */

use anyhow::Result;
use dialoguer::{FuzzySelect, Select, theme::ColorfulTheme};

pub enum Approval {
    Once,
    Always,
    Decline,
    Stop,
}

/// Asks whether to run `tool`. Esc declines the call; Ctrl+C stops the run.
pub async fn confirm_tool(tool: String) -> Approval {
    let ask = move || {
        Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Run `{tool}`?"))
            .items(["Yes", &format!("Always allow `{tool}` this session"), "No"])
            .default(0)
            .report(false)
            .interact_opt()
    };
    match tokio::task::spawn_blocking(ask).await {
        Ok(Ok(Some(0))) => Approval::Once,
        Ok(Ok(Some(1))) => Approval::Always,
        Ok(Ok(_)) => Approval::Decline,
        _ => Approval::Stop,
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
