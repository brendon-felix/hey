/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::{ChatCompletionRequestMessage, CreateChatCompletionRequest};
use yansi::Paint;

#[derive(Debug, PartialEq)]
enum LoopControl {
    Continue,
    Exit,
}
use crate::commands::Command;
use crate::config::Config;
use crate::conversation::Conversation;
use crate::editor::{Editor, EditorConfig, Input};
use crate::render::{Highlighter, snailprint};
use crate::response::{create_request, generate_title, stream_response};
use crate::utils::{
    clear_console, print_help, print_sample_text, print_separator, select_filename,
    select_json_file, select_model, select_theme,
};

pub struct ReadEvalPrintLoop {
    client: Client<OpenAIConfig>,
    model: String,
    max_tokens: u32,
    conversation: Conversation,
    editor: Editor,
    theme: String,
    syntax_highlighting: bool,
    history_file: Option<String>,
    greetings: bool,
    wrap_width: u32,
    conversations_folder: String,
}

impl ReadEvalPrintLoop {
    pub fn new(client: Client<OpenAIConfig>, config: Config) -> Self {
        let editor_config = EditorConfig::from_config(&config);
        let editor = Editor::new(editor_config);
        let model = config.model;
        let max_tokens = config.max_tokens;
        let conversation = Conversation::new(config.system_prompt);
        let theme = config.theme;
        let syntax_highlighting = config.syntax_highlighting;
        let history_file = None;
        let greetings = config.greetings;
        let wrap_width = config.wrap_width;
        let conversations_folder = config.conversations_folder;
        Self {
            client,
            model,
            max_tokens,
            conversation,
            editor,
            theme,
            syntax_highlighting,
            history_file,
            greetings,
            wrap_width,
            conversations_folder,
        }
    }

    pub fn with_conversation(
        client: Client<OpenAIConfig>,
        config: Config,
        conversation: Conversation,
        // history_file: Option<String>,
    ) -> Self {
        let editor_config = EditorConfig::from_config(&config);
        let editor = Editor::new(editor_config);
        let model = config.model;
        let max_tokens = config.max_tokens;
        let theme = config.theme;
        let syntax_highlighting = config.syntax_highlighting;
        let history_file = None;
        let greetings = config.greetings;
        let wrap_width = config.wrap_width;
        let conversations_folder = config.conversations_folder;
        Self {
            client,
            model,
            max_tokens,
            conversation,
            editor,
            theme,
            syntax_highlighting,
            history_file,
            greetings,
            wrap_width,
            conversations_folder,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        self.check_conversations_folder_on_startup();

        match self.conversation.messages.last().unwrap() {
            ChatCompletionRequestMessage::User(_) => {
                let request = create_request(
                    &self.model,
                    self.max_tokens,
                    self.conversation.messages.clone(),
                )?;
                let response = match self.get_response(request).await {
                    Err(e) => {
                        snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                        String::new()
                    }
                    Ok(response) => response,
                };
                self.conversation.add_assistant_message(response);
            }
            _ => {
                if self.greetings {
                    snailprint("\nHey!\n\n", 10000);
                } else {
                    println!();
                }
            }
        }
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Message(message) => {
                    self.conversation.add_user_message(message);
                    let request = create_request(
                        &self.model,
                        self.max_tokens,
                        self.conversation.messages.clone(),
                    )?;
                    let response = match self.get_response(request).await {
                        Err(e) => {
                            snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                            continue;
                        }
                        Ok(response) => response,
                    };
                    self.conversation.add_assistant_message(response);
                }
                Input::Command(command) => match self.handle_command(command).await? {
                    LoopControl::Exit => break,
                    LoopControl::Continue => {}
                },
                Input::Invalid => {}
            }
        }
        Ok(())
    }

    async fn handle_command(&mut self, command: Command) -> Result<LoopControl> {
        match command {
            Command::Exit => {
                if self.greetings {
                    snailprint(&format!("\n{}\n\n", "Bye!"), 10000);
                } else {
                    println!();
                }
                return Ok(LoopControl::Exit);
            }
            Command::Clear => {
                snailprint(&format!("\n{}\n\n", "Clearing...".bold()), 5000);
                sleep(Duration::from_millis(250));
                clear_console();
            }
            Command::Reset => {
                snailprint(
                    &format!("\n{}\n\n", "Resetting conversation...".bold()),
                    5000,
                );
                sleep(Duration::from_millis(500));
                self.conversation.reset();
            }
            Command::SelectModel => {
                let selection = select_model(&self.model)?;
                self.model = selection;
            }
            Command::SelectTheme => {
                let selection = select_theme()?;
                self.theme = selection;
                if let Err(e) = print_sample_text(&self.theme, self.wrap_width) {
                    snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                }
            }
            Command::Save => {
                if self.conversation.messages.len() <= 1 {
                    snailprint(
                        &format!(
                            "\n{} No conversation history to save.\n\n",
                            "Info:".yellow()
                        ),
                        5000,
                    );
                } else {
                    self.save_conversation().await?;
                }
            }
            Command::Load => {
                self.load_conversation()?;
                print_separator();
                self.print_conversation();
            }
            Command::History => {
                self.print_conversation();
            }
            Command::Help => {
                print_help();
            }
            Command::Invalid => {
                snailprint(
                    &format!(
                        "\nInvalid command. Type /{} for a list of commands.\n\n",
                        "help".cyan()
                    ),
                    2000,
                );
            }
        }
        Ok(LoopControl::Continue)
    }

    pub async fn get_response(&mut self, request: CreateChatCompletionRequest) -> Result<String> {
        let mut highlighter = if self.syntax_highlighting {
            Some(Highlighter::new(&self.theme)?)
        } else {
            None
        };
        stream_response(&self.client, request, &mut highlighter, self.wrap_width).await
    }

    async fn save_conversation(&self) -> Result<()> {
        let conversations_folder = match self.get_conversations_folder_for_operation() {
            Ok(folder) => folder,
            Err(_) => return Ok(()),
        };

        let filename = if let Some(filename) = &self.history_file {
            filename.clone()
        } else {
            let generated_title =
                generate_title(&self.client, self.conversation.transcript()).await?;
            select_filename(generated_title)?
        };

        let filepath = std::path::Path::new(&conversations_folder).join(&filename);
        let filepath_str = filepath.to_string_lossy().to_string();

        // Ensure the conversations folder exists before saving
        if let Some(parent) = filepath.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| "Failed to create conversations directory")?;
        }

        self.conversation.save_to_json_file(&filepath_str)?;
        snailprint(
            &format!(
                "\n{} {}.\n\n",
                "Conversation saved successfully to".green(),
                filepath_str.blue()
            ),
            2000,
        );
        Ok(())
    }

    fn load_conversation(&mut self) -> Result<()> {
        let conversations_folder = match self.get_conversations_folder_for_operation() {
            Ok(folder) => folder,
            Err(_) => return Ok(()),
        };

        let filename = match select_json_file(&conversations_folder) {
            Ok(None) => {
                bail!("Load cancelled.");
            }
            Ok(Some(filename)) => filename,
            Err(e) => {
                return Err(e);
            }
        };
        self.history_file = Some(filename.clone());
        self.conversation = Conversation::from_json_file(&filename)?;
        Ok(())
    }

    fn check_conversations_folder_on_startup(&self) {
        use std::path::Path;

        let folder_path = match self.expand_path(&self.conversations_folder) {
            Ok(path) => path,
            Err(_) => self.conversations_folder.clone(),
        };
        if !Path::new(&folder_path).exists() {
            snailprint(
                &format!(
                    "\n{} The configured conversations folder '{}' does not exist.\n",
                    "Warning:".yellow(),
                    folder_path.yellow()
                ),
                2000,
            );
            snailprint(
                &format!(
                    "{} When saving or loading conversations, you'll be prompted to use the current directory.\n\n",
                    "Note:".blue()
                ),
                2000,
            );
        }
    }

    fn get_conversations_folder_for_operation(&self) -> Result<String> {
        use dialoguer::Confirm;
        use std::path::Path;

        let folder_path = self.expand_path(&self.conversations_folder)?;

        if Path::new(&folder_path).exists() {
            Ok(folder_path)
        } else {
            snailprint(
                &format!(
                    "\n{} The configured conversations folder '{}' does not exist.\n",
                    "Warning:".yellow(),
                    folder_path.yellow()
                ),
                2000,
            );

            let use_current_dir = Confirm::new()
                .with_prompt("Would you like to use the current working directory instead?")
                .default(true)
                .interact()?;

            if use_current_dir {
                println!();
                Ok(String::from("./"))
            } else {
                snailprint(
                    &format!("\n{} Operation cancelled.\n\n", "Info:".blue()),
                    2000,
                );
                bail!("Operation cancelled by user");
            }
        }
    }

    fn expand_path(&self, path: &str) -> Result<String> {
        use std::path::Path;

        let path = path.trim();

        // Use shellexpand for robust cross-platform path expansion
        let expanded =
            shellexpand::full(path).map_err(|e| anyhow::anyhow!("Path expansion failed: {}", e))?;

        // Normalize the path to handle both forward and back slashes
        let path_buf = Path::new(expanded.as_ref()).to_path_buf();

        // Convert to absolute path if it's relative
        let normalized = if path_buf.is_absolute() {
            path_buf
        } else {
            std::env::current_dir()
                .with_context(|| "Failed to get current directory")?
                .join(path_buf)
        };

        // Convert to string with proper path separators for the platform
        Ok(normalized.to_string_lossy().to_string())
    }

    fn print_conversation(&self) {
        if self.conversation.messages.len() <= 1 {
            snailprint("\nNo conversation history available.\n\n", 5000);
            return;
        }
        let mut highlighter = if self.syntax_highlighting {
            Highlighter::new(&self.theme).ok()
        } else {
            None
        };
        self.conversation
            .print_messages(&mut highlighter, self.wrap_width);
        println!();
    }
}
