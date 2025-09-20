// FIX: Highlight "Hey!" according to the set theme
// FIX: Prevent saving conversations with no history
// FIX: Generate up-to-date syntax set

// TODO: Generate up-to-date syntax highlighting assests (when needed)
// TODO: Support deserialized TOML configuration file
// TODO: Add load flag to load a conversation in comand line (--load, -l)
// TODO: Add a context flag to load a file as context (--context, -c)
// TODO: Implement auto saving and loading (default off)
// TODO: Support models like o3 and gpt-5
// TODO: Add file referencing for current working directory (/context, /c)
// TODO: Add config setting for conversations folder
// TODO: Try using function calling
// TODO: Save and Load Reedline line editor history
// TODO: Allow editor as prompt input as supported by reedline
// TODO: Support command tab completion for commands
// TODO: Experiment with ratatui based UI
// TODO: Allow for user to change edit mode (emacs)
// TODO: Use nushell $env to configure reedline

// Configuration options:
// - API key path
// - API key env var
// - System prompt
// - System prompt path
// - Model
// - Max tokens
// - Single message or REPL for args message
// - Auto save/load conversations
// - Disable snail print
// - Conversations folder
// - Configure reedline prompt
//   - edit mode (emacs, vi)
//   - command tab completion
//   - input editor
//   - highlighting
//   - prompt style (color, indicator, etc)
//   - history file path
//   - history size
// - Configure response rendering
//   - disable syntax highlighting
//   - disable ansi color
//   - default theme
//   - wrap width ('auto' or int)

use anyhow::{Context, Result};
use async_openai::{Client, config::OpenAIConfig};
use clap::Parser;
use yansi::Paint;

mod commands;
mod config;
mod conversation;
mod editor;
mod render;
mod repl;
mod response;
mod utils;

use config::{Config, get_config_toml};

#[derive(Parser, Debug)]
struct Args {
    /// Optional path to a system prompt text file
    #[arg(long, short)]
    prompt_path: Option<String>,

    /// Message to send to the model
    /// - Enters the REPL if `enter_repl` is set to true
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    message: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let prompt: Option<String> = if let Some(path) = args.prompt_path {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read system prompt at path {}", path.blue()))
            .ok()
    } else {
        None
    };
    let config = if let Some(config_toml) = get_config_toml() {
        Config::from_toml(config_toml, prompt)
    } else {
        Config::default()
    };

    let api_key = std::env::var("OPENAI_API_KEY")
        .context("Please set the OPENAI_API_KEY environment variable to your OpenAI API key.")?;

    // let api_key = match args.api_key_path {
    //     Some(path) => std::fs::read_to_string(path)
    //         .context("Failed to read API key from file")?
    //         .trim()
    //         .to_string(),
    //     None => std::env::var("OPENAI_API_KEY").context(
    //         "Please set the OPENAI_API_KEY environment variable to your OpenAI API key.",
    //     )?,
    // };

    // utils::api_check(&api_key).await?;

    // let system_prompt = if let Some(path) = args.prompt_path {
    //     std::fs::read_to_string(&path)
    //         .with_context(|| format!("Failed to read system prompt at path {}", path.blue()))?
    // } else {
    //     println!(
    //         "{}",
    //         "No system prompt file provided, using default.".yellow()
    //     );
    //     String::from(DEFAULT_SYSTEM_PROMPT)
    // };

    let openai_config = OpenAIConfig::new().with_api_key(api_key);
    let client = Client::with_config(openai_config);

    if args.message.is_empty() {
        // interactive REPL
        let mut repl = repl::ReadEvalPrintLoop::new(client, config);
        repl.run().await?;
    } else if config.enter_repl {
        // enter REPL with initial message
        let messages = vec![
            utils::new_system_message(config.system_prompt.clone()),
            utils::new_user_message(args.message.join(" ")),
        ];
        let mut repl = repl::ReadEvalPrintLoop::with_conversation(
            client,
            config,
            conversation::Conversation::from_messages(messages),
        );
        repl.run().await?;
    } else {
        // single message
        let messages = vec![
            utils::new_system_message(config.system_prompt),
            utils::new_user_message(args.message.join(" ")),
        ];
        let request = response::create_request(&config.model, config.max_tokens, messages)?;
        let mut highlighter = render::Highlighter::new(&config.theme)?;
        response::stream_response(&client, request, &mut highlighter).await?;
    }
    Ok(())
}
