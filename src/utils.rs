/* -------------------------------------------------------------------------- */
/*                                  utils.rs                                  */
/* -------------------------------------------------------------------------- */

use async_openai::types::{
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
};
use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use term_size;
use yansi::Paint;

use crate::render::{Highlighter, wrap_line};

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

pub fn select_json_file(dir_path: &str) -> Result<Option<String>> {
    let entries = std::fs::read_dir(dir_path)
        .with_context(|| format!("Failed to read directory {}", dir_path))?;
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
    println!();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a JSON file to load")
        .items(&options)
        .default(0)
        .interact()
        .context("Failed to read input")?;
    println!();

    let filepath = json_files[selection].to_string_lossy().to_string();

    Ok(Some(filepath))
}

pub fn select_filename(generated_title: String) -> Result<String> {
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

pub fn select_model(default: &str) -> Result<String> {
    let models = vec![
        "gpt-3.5-turbo",
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4.1",
        "gpt-5",
        "o3",
    ];
    let default_index = models.iter().position(|&m| m == default).unwrap_or(0);
    println!();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a model")
        .items(&models)
        .default(default_index)
        .interact()?;
    println!();
    Ok(models[selection].to_string())
}

pub fn select_theme() -> Result<String> {
    let assets = bat::assets::HighlightingAssets::from_binary();
    let themes = assets.themes().collect::<Vec<&str>>();
    println!();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a theme")
        .items(&themes)
        .default(0)
        .interact()?;
    println!();
    Ok(themes[selection].to_string())
}

pub fn print_sample_text(theme_name: &str) -> Result<()> {
    let sample_text = r#"# Sample text with code:

- Rust **function** for computing _factorial_:

    ```rust
    fn factorial(n: u32) -> u32 {
        if n == 0 { 1 } else { n * factorial(n - 1) }
    }
    ```
- Python **class** for an _API client_:

    ```python
    class APIClient:
        def __init__(self, api_key):
            self.api_key = api_key
    ```
"#;
    let mut highlighter = Highlighter::new(theme_name)?;
    for line in sample_text.split_inclusive("\n") {
        let line = highlighter.highlight_line(line);
        let line = wrap_line(&line);
        print!("{}", line);
    }
    println!();
    Ok(())
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
