/* -------------------------------------------------------------------------- */
/*                                  utils.rs                                  */
/* -------------------------------------------------------------------------- */

use async_openai::types::{
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
};
use dialoguer::{Confirm, Input, Select};
use term_size;
use yansi::Paint;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const SUPPORTED_MODELS: [&str; 3] = ["gpt-3.5-turbo", "gpt-4o", "gpt-4o-mini"];

pub fn clear_console() {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "cls"])
            .status();
    } else {
        let _ = std::process::Command::new("clear").status();
    }
}

pub fn print_separator() {
    let term_width = term_size::dimensions().map(|(w, _)| w).unwrap_or(80);
    let separator = "─".repeat(term_width);
    println!("{}", separator);
}

// pub async fn api_check(api_key: &str) -> Result<(), String> {
//     let client = reqwest::Client::new();
//     let response = client
//         .get("https://api.openai.com/v1/models")
//         .header("Authorization", format!("Bearer {}", api_key))
//         .send();

//     match response.await {
//         Ok(resp) if resp.status().is_success() => Ok(()),
//         Ok(resp) => Err(format!("Failed to reach OpenAI API: {}", resp.status())),
//         Err(e) => Err(format!("Error reaching OpenAI API: {}", e)),
//     }
// }

pub fn get_prompt(path: Option<String>) -> String {
    match path {
        Some(p) => std::fs::read_to_string(&p)
            .unwrap_or_else(|_| panic!("Failed to read system prompt at path {}", p)),
        None => {
            println!(
                "{}",
                "No system prompt file provided, using default.".yellow()
            );
            String::from(DEFAULT_SYSTEM_PROMPT)
        }
    }
}

pub fn select_json_file(dir_path: &str) -> Result<Option<String>, String> {
    let entries = std::fs::read_dir(dir_path)
        .map_err(|e| format!("Failed to read directory {}: {}", dir_path, e))?;
    let json_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "json"))
        .map(|e| e.path())
        .collect();

    if json_files.is_empty() {
        println!("{}", "No JSON files found.".yellow());
        return Ok(None);
    }

    let options: Vec<String> = json_files
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
                .blue()
                .to_string()
        })
        .collect();

    let selection = Select::new()
        .with_prompt("\nSelect a JSON file to load")
        .items(&options)
        .default(0)
        .interact()
        .map_err(|e| format!("Failed to read input: {}", e))?;

    let filepath = json_files[selection].to_string_lossy().to_string();

    Ok(Some(filepath))
}

pub fn select_filename(generated_title: String) -> Result<String, Box<dyn std::error::Error>> {
    let title = generated_title;
    let title_confirmed = Confirm::new()
        .with_prompt(&format!(
            "\nDo you want to save the conversation with the title: {}?",
            title.blue()
        ))
        .default(true)
        .interact()?;
    let title: String = if title_confirmed {
        title
    } else {
        Input::new()
            .with_prompt("Enter a filename or title for the conversation")
            .interact_text()?
    };
    let name = if title.is_empty() {
        format!(
            "conversation_{}",
            chrono::Utc::now().format("%Y%m%d_%H%M%S")
        )
    } else {
        format!(
            "{}",
            title
                .trim()
                .to_lowercase()
                .replace(' ', "_")
                .replace(", ", "_")
        )
    };
    if name.ends_with(".json") {
        Ok(name)
    } else {
        Ok(format!("{}.json", name))
    }
}

pub fn select_model(default: &str) -> Result<String, Box<dyn std::error::Error>> {
    let models = &SUPPORTED_MODELS;
    let default_index = models.iter().position(|&m| m == default).unwrap_or(0);
    let selection = Select::new()
        .with_prompt("Select a model")
        .items(models)
        .default(default_index)
        .interact()?;
    Ok(models[selection].to_string())
}

pub fn new_system_message(content: String) -> ChatCompletionRequestMessage {
    ChatCompletionRequestSystemMessageArgs::default()
        .content(content)
        .build()
        .unwrap()
        .into()
}

pub fn new_user_message(content: String) -> ChatCompletionRequestMessage {
    ChatCompletionRequestUserMessageArgs::default()
        .content(content)
        .build()
        .unwrap()
        .into()
}

pub fn new_assistant_message(content: String) -> ChatCompletionRequestMessage {
    ChatCompletionRequestAssistantMessageArgs::default()
        .content(content)
        .build()
        .unwrap()
        .into()
}

// fn create_nametag(name: &str) -> String {
//     let length = name.to_string().len();
//     let name = Paint::new(name).magenta().italic();
//     format!(
//         "╭{}╮\n│ {} │\n╰{}╯",
//         "─".repeat(length + 2),
//         name,
//         "─".repeat(length + 2)
//     )
// }
