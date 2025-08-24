// #[derive(Deserialize, Debug)]
// struct Config {
//     name: Option<String>,
//     api_key: Option<String>,
//     prompt: Option<String>,
//     model: String,
//     temperature: f32,
//     max_tokens: u32,
// }

// fn load() {
// let config = std::fs::read_to_string("config.toml")
//     .map_err(|_| "Failed to read config.toml")?;
// let config: Config = toml::from_str(&config)?;
// let system_prompt = config.prompt
//     .unwrap_or_else(|| String::from(DEFAULT_SYSTEM_PROMPT));
// }
