use std::fs::File;
use std::io::Read;
use chatgpt::prelude::*;
use clap::Parser as ArgParser;

mod commands;
mod conversation;
use conversation::{stream_single_response, conversation};

#[derive(ArgParser, Debug)]
struct Args {
    /// Path to the API key file
    #[arg(long, short, default_value = "api_key.txt")]
    api_key: String,

    /// Path to the system prompt file
    #[arg(long, short, default_value = "system_prompt.txt")]
    prompt_path: String,

    /// Single message to send to the model
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let api_key: String = File::open(args.api_key)
        .and_then(|mut file| {
            let mut key = String::new();
            file.read_to_string(&mut key)?;
            Ok(key)
        })
        .unwrap_or_else(|_| {
            eprintln!("Failed to read API key from file. Please ensure 'api_key.txt' exists.");
            std::process::exit(1);
        });

    let config = ModelConfiguration {
        engine: ChatGPTEngine::Gpt35Turbo,
        ..Default::default()
    };
    let client = ChatGPT::new_with_config(api_key, config)?;

    if !args.args.is_empty() {
        // If a single message is provided, send it to the model
        let message = args.args.join(" ");
        stream_single_response(&client, message, args.prompt_path).await?;
    } else {
        // If no message is provided, start a conversation
        conversation(&client, args.prompt_path).await?;
    }
    Ok(())
}
