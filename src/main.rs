// TODO: Add Load and Store conversation history commands
// TODO: Support for model setting
// TODO: Try using function calling
// TODO: Support deserialized TOML configuration file
// TODO: Save and Load Reedline line editor history
// TODO: Support command tab completion
// TODO: Experiment with ratatui based UI

use clap::Parser;
use yansi::Paint;
// use toml;
// use serde::Deserialize;

mod app;
mod commands;
mod editor;
mod render;
mod utils;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

// #[derive(Deserialize, Debug)]
// struct Config {
//     name: Option<String>,
//     api_key: Option<String>,
//     prompt: Option<String>,
//     model: String,
//     temperature: f32,
//     max_tokens: u32,
// }

#[derive(Parser, Debug)]
struct Args {
    ///// Path to the API key file
    //#[arg(long, short)]
    //api_key: String,

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

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| {
            "Please set the OPENAI_API_KEY environment variable to your OpenAI API key."
        })?;

    let system_prompt = get_prompt(args.prompt_path);

    // let config = std::fs::read_to_string("config.toml")
    //     .map_err(|_| "Failed to read config.toml")?;
    // let config: Config = toml::from_str(&config)?;
    // let system_prompt = config.prompt
    //     .unwrap_or_else(|| String::from(DEFAULT_SYSTEM_PROMPT));
 
    let mut app = app::App::new(&api_key, system_prompt);

    if args.message.is_empty() {
        // let name = "RustyGPT";
        // app.print_nametag(name);
        app.run().await?;
    } else {
        app.get_response(args.message.join(" ")).await?;
    }
    Ok(())
}

fn get_prompt(path: Option<String>) -> String {
    match path {
        Some(p) => std::fs::read_to_string(&p).unwrap_or_else(|_| {
            panic!("Failed to read system prompt at path {}", p)
        }),
        None => {
            println!("{}", "No system prompt file provided, using default.".yellow());
            String::from(DEFAULT_SYSTEM_PROMPT)
        }
    }
}

