/* -------------------------------------------------------------------------- */
/*                                  utils.rs                                  */
/* -------------------------------------------------------------------------- */

use term_size;
use yansi::Paint;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";

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

pub async fn api_check(api_key: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.openai.com/v1/models")
        .header("Authorization", format!("Bearer {}", api_key))
        .send();

    match response.await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("Failed to reach OpenAI API: {}", resp.status())),
        Err(e) => Err(format!("Error reaching OpenAI API: {}", e)),
    }
}

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
