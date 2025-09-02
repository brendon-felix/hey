// FIX: Highlight "Hey!" according to the set theme
// FIX: Prevent saving conversations with no history
// FIX: Generate up-to-date syntax set

// TODO: Support models like o3 and gpt-5
// TODO: Add file referencing for current working directory
// TODO: Add config setting for conversations folder
// TODO: Try using function calling
// TODO: Support deserialized TOML configuration file
// TODO: Save and Load Reedline line editor history
// TODO: Support command tab completion for commands
// TODO: Experiment with ratatui based UI
// TODO: Allow for user to change edit mode (emacs)
// TODO: Use nushell $env to configure reedline

use anyhow::{Context, Result};
use async_openai::{Client, config::OpenAIConfig};
use clap::Parser;
use yansi::Paint;

mod commands;
mod conversation;
mod editor;
mod render;
mod repl;
mod response;
mod utils;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_MODEL: &str = "gpt-4o";
const DEFAULT_THEME: &str = "ansi";
const DEFAULT_MAX_TOKENS: u32 = 2048;

#[derive(Parser, Debug)]
struct Args {
    // Path to the API key file
    #[arg(long, short)]
    api_key_path: Option<String>,

    /// Path to the system prompt file
    #[arg(long, short)]
    prompt_path: Option<String>,

    /// Single message to send to the model
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    message: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let api_key = match args.api_key_path {
        Some(path) => std::fs::read_to_string(path)
            .context("Failed to read API key from file")?
            .trim()
            .to_string(),
        None => std::env::var("OPENAI_API_KEY").context(
            "Please set the OPENAI_API_KEY environment variable to your OpenAI API key.",
        )?,
    };

    // utils::api_check(&api_key).await?;

    let system_prompt = if let Some(path) = args.prompt_path {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read system prompt at path {}", path.blue()))?
    } else {
        println!(
            "{}",
            "No system prompt file provided, using default.".yellow()
        );
        String::from(DEFAULT_SYSTEM_PROMPT)
    };

    let openai_config = OpenAIConfig::new().with_api_key(api_key);
    let client = Client::with_config(openai_config);

    if args.message.is_empty() {
        // interactive REPL
        let mut repl = repl::ReadEvalPrintLoop::new(client, system_prompt);
        repl.run().await?;
    } else {
        // single message
        let messages = vec![
            utils::new_system_message(system_prompt),
            utils::new_user_message(args.message.join(" ")),
        ];
        let request = response::create_request(DEFAULT_MODEL, DEFAULT_MAX_TOKENS, messages)?;
        let mut highlighter = render::Highlighter::new(DEFAULT_THEME)?;
        response::stream_response(&client, request, &mut highlighter).await?;
    }
    Ok(())
}
