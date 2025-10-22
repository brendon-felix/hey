use anyhow::{Context, Result};
use dirs::config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use toml;
use yansi::Paint;

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
//   - wrap width (int, 0 to disable)

const DEFAULT_ENTER_REPL: bool = false;
// const DEFAULT_AUTO_SAVE: bool = false;
// const DEFAULT_AUTO_LOAD: bool = false;
const DEFAULT_ANIMATIONS: bool = false;
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const DEFAULT_MODEL: &str = "gpt-4o";
const DEFAULT_MAX_TOKENS: u32 = 2048;
const DEFAULT_BRACKETED_PASTE: bool = true;
const DEFAULT_ANSI_COLORS: bool = true;
const DEFAULT_EDIT_MODE: &str = "emacs";
const DEFAULT_SYNTAX_HIGHLIGHTING: bool = true;
const DEFAULT_THEME: &str = "ansi";
const DEFAULT_WRAP_WIDTH: u32 = 100;

pub struct Config {
    pub system_prompt: String,
    pub model: String,
    pub max_tokens: u32,
    pub enter_repl: bool,
    // pub auto_save: bool,
    // pub auto_load: bool,
    pub animations: bool, // snail print
    pub bracketed_paste: bool,
    pub ansi_colors: bool,
    pub edit_mode: String,
    pub syntax_highlighting: bool,
    pub theme: String,
    pub wrap_width: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            system_prompt: String::from(DEFAULT_SYSTEM_PROMPT),
            model: String::from(DEFAULT_MODEL),
            max_tokens: DEFAULT_MAX_TOKENS,
            enter_repl: DEFAULT_ENTER_REPL,
            // auto_save: DEFAULT_AUTO_SAVE,
            // auto_load: DEFAULT_AUTO_LOAD,
            animations: DEFAULT_ANIMATIONS,
            bracketed_paste: DEFAULT_BRACKETED_PASTE,
            ansi_colors: DEFAULT_ANSI_COLORS,
            edit_mode: String::from(DEFAULT_EDIT_MODE),
            syntax_highlighting: DEFAULT_SYNTAX_HIGHLIGHTING,
            theme: String::from(DEFAULT_THEME),
            wrap_width: DEFAULT_WRAP_WIDTH,
        }
    }
}

impl Config {
    pub fn from_toml(config_toml: ConfigToml, prompt: Option<String>) -> Self {
        let system_prompt = prompt.unwrap_or_else(|| match config_toml.system_prompt {
            Some(prompt) => prompt,
            None => {
                println!(
                    "{}",
                    "No system prompt file provided, using default.".yellow()
                );
                String::from(DEFAULT_SYSTEM_PROMPT)
            }
        });
        let model = config_toml.model.unwrap_or(String::from(DEFAULT_MODEL));
        let max_tokens = config_toml.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let enter_repl = config_toml.enter_repl.unwrap_or(DEFAULT_ENTER_REPL);
        // let auto_save = config_toml.auto_save.unwrap_or(DEFAULT_AUTO_SAVE);
        // let auto_load = config_toml.auto_load.unwrap_or(DEFAULT_AUTO_LOAD);
        let animations = config_toml.animations.unwrap_or(DEFAULT_ANIMATIONS);
        let bracketed_paste = config_toml
            .bracketed_paste
            .unwrap_or(DEFAULT_BRACKETED_PASTE);
        let ansi_colors = config_toml.ansi_colors.unwrap_or(DEFAULT_ANSI_COLORS);
        let edit_mode = config_toml
            .edit_mode
            .unwrap_or(String::from(DEFAULT_EDIT_MODE));
        let syntax_highlighting = config_toml
            .syntax_highlighting
            .unwrap_or(DEFAULT_SYNTAX_HIGHLIGHTING);
        let theme = config_toml.theme.unwrap_or(String::from(DEFAULT_THEME));
        let wrap_width = config_toml.wrap_width.unwrap_or(DEFAULT_WRAP_WIDTH);

        Self {
            // api_key,
            system_prompt,
            model,
            max_tokens,
            enter_repl,
            // auto_save,
            // auto_load,
            animations,
            bracketed_paste,
            ansi_colors,
            edit_mode,
            syntax_highlighting,
            theme,
            wrap_width,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ConfigToml {
    // api_key: Option<String>,
    system_prompt: Option<String>,
    model: Option<String>,
    max_tokens: Option<u32>,
    enter_repl: Option<bool>,
    auto_save: Option<bool>,
    auto_load: Option<bool>,
    animations: Option<bool>, // snail print
    bracketed_paste: Option<bool>,
    ansi_colors: Option<bool>,
    edit_mode: Option<String>,
    syntax_highlighting: Option<bool>,
    theme: Option<String>,
    wrap_width: Option<u32>,
}

impl ConfigToml {
    pub fn load(path: PathBuf) -> Result<Self> {
        let config = fs::read_to_string(path).context("Failed to read config file")?;
        let config: Self = toml::from_str(&config).context("Invalid config file")?;
        Ok(config)
    }
}

fn get_config_path() -> Result<PathBuf> {
    // Configuration directory path
    // ╭──────────┬───────────────────────────────────╮
    // │ Platform │               Path                │
    // ├──────────┼───────────────────────────────────┤
    // │ Linux    │ $XDG_CONFIG_HOME or $HOME/.config │
    // │ macOS    │ $HOME/Library/Application Support │
    // │ Windows  │ %APPDATA%                         │
    // ╰──────────┴───────────────────────────────────╯
    let config_toml_path = config_dir()
        .map(|path| path.join("hey").join("hey.toml"))
        .context("Failed to determine config path")?;
    Ok(config_toml_path)
}

pub fn get_config_toml() -> Option<ConfigToml> {
    let config_path = get_config_path().ok()?;
    if config_path.exists() {
        ConfigToml::load(config_path).ok()
    } else {
        None
    }
}
