/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::ChatCompletionRequestMessage;
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
    conversation: Conversation,
    editor: Editor,
    theme: String,
    syntax_highlighting: bool,
    history_file: Option<String>,
    wrap_width: u32,
}

impl ReadEvalPrintLoop {
    pub fn new(client: Client<OpenAIConfig>, config: Config) -> Self {
        let editor_config = EditorConfig::from_config(&config);
        let editor = Editor::new(editor_config);
        let model = config.model;
        let conversation = Conversation::new(config.system_prompt);
        let theme = config.theme;
        let syntax_highlighting = config.syntax_highlighting;
        let history_file = None;
        let wrap_width = config.wrap_width;
        Self {
            client,
            model,
            conversation,
            editor,
            theme,
            syntax_highlighting,
            history_file,
            wrap_width,
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
        let theme = config.theme;
        let syntax_highlighting = config.syntax_highlighting;
        let history_file = None;
        let wrap_width = config.wrap_width;
        Self {
            client,
            model,
            conversation,
            editor,
            theme,
            syntax_highlighting,
            history_file,
            wrap_width,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        match self.conversation.messages.last().unwrap() {
            ChatCompletionRequestMessage::User(_) => {
                let response = match self.get_response().await {
                    Err(e) => {
                        snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                        String::new()
                    }
                    Ok(response) => response,
                };
                self.conversation.add_assistant_message(response);
            }
            _ => {
                snailprint("\nHey!\n\n", 10000);
            }
        }
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Message(message) => {
                    self.conversation.add_user_message(message);
                    let response = match self.get_response().await {
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
                snailprint(&format!("\n{}\n\n", "Exiting...".red()), 5000);
                sleep(Duration::from_millis(250));
                return Ok(LoopControl::Exit);
            }
            Command::Clear => {
                snailprint(&format!("\n{}\n\n", "Clearing...".yellow()), 5000);
                sleep(Duration::from_millis(250));
                clear_console();
            }
            Command::Reset => {
                snailprint(
                    &format!("\n{}\n", "Resetting conversation...".yellow()),
                    5000,
                );
                println!();
                print_separator();
                println!();
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
                self.save_conversation().await?;
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

    pub async fn get_response(&mut self) -> Result<String> {
        let request = create_request(&self.model, 2048u32, self.conversation.messages.clone())?;
        let mut highlighter = if self.syntax_highlighting {
            Some(Highlighter::new(&self.theme)?)
        } else {
            None
        };
        stream_response(&self.client, request, &mut highlighter, self.wrap_width).await
    }

    async fn save_conversation(&self) -> Result<()> {
        let filename = if let Some(filename) = &self.history_file {
            filename.clone()
        } else {
            let generated_title =
                generate_title(&self.client, self.conversation.transcript()).await?;
            select_filename(generated_title)?
        };
        let filepath = filename;
        self.conversation.save_to_json_file(&filepath)?;
        snailprint(
            &format!(
                "\n{} {}.\n\n",
                "Conversation saved successfully to".green(),
                filepath.blue()
            ),
            2000,
        );
        Ok(())
    }

    fn load_conversation(&mut self) -> Result<()> {
        let conversations_folder = "./";

        let filename = match select_json_file(conversations_folder) {
            Ok(None) => {
                bail!("Load cancelled.");
            }
            Ok(Some(filename)) => filename,
            Err(e) => {
                return Err(e);
            }
        };
        let filepath = filename; // need to add config setting for conversations folder
        self.history_file = Some(filepath.clone());
        self.conversation = Conversation::from_json_file(&filepath)?;
        Ok(())
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
