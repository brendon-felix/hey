// TODO: Support for model setting
// TODO: Try using function calling
// TODO: Support deserialized TOML configuration file
// TODO: Save and Load Reedline line editor history
// TODO: Support command tab completion
// TODO: Experiment with ratatui based UI
// TODO: Allow for user to change edit mode (emacs)
// TODO: Use nushell $env to configure reedline

use async_openai::Client;
use clap::Parser;

// use yansi::Paint;
// use toml;
// use serde::Deserialize;

mod app;
mod commands;
mod conversation;
mod editor;
mod render;
mod response;
mod utils;

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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // let api_key = match args.api_key_path {
    //     Some(path) => std::fs::read_to_string(path)
    //         .map_err(|_| "Failed to read API key from file")?
    //         .trim()
    //         .to_string(),
    //     None => std::env::var("OPENAI_API_KEY").map_err(
    //         |_| "Please set the OPENAI_API_KEY environment variable to your OpenAI API key.",
    //     )?,
    // };

    // utils::api_check(&api_key).await?;

    let system_prompt = utils::get_prompt(args.prompt_path);

    if args.message.is_empty() {
        let mut app = app::App::new(system_prompt);
        app.run().await?;
    } else {
        let client = Client::new();
        let messages = vec![
            utils::new_system_message(system_prompt),
            utils::new_user_message(args.message.join(" ")),
        ];
        let request = response::create_request("gpt-4o", 2048u32, messages)?;
        let highlighter = &mut render::Highlighter::new();
        response::stream_response(&client, request, highlighter).await?;
    }
    Ok(())
}
