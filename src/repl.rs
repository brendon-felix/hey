/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use yansi::Paint;

use crate::commands::Command;

#[derive(Debug, PartialEq)]
enum LoopControl {
    Continue,
    Exit,
}
use crate::conversation::Conversation;
use crate::editor::{Editor, Input};
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
    history_file: Option<String>,
}

impl ReadEvalPrintLoop {
    pub fn new(client: Client<OpenAIConfig>, system_prompt: String) -> Self {
        let model = "gpt-4o".to_string();
        let conversation = Conversation::new(system_prompt);
        let editor = Editor::new();
        let theme = "ansi".to_string();
        let history_file = None;
        Self {
            client,
            model,
            conversation,
            editor,
            theme,
            history_file,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        snailprint("\nHey!\n\n", 10000);
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
                if let Err(e) = print_sample_text(&self.theme) {
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
        let mut highlighter = Highlighter::new(&self.theme)?;
        stream_response(&self.client, request, &mut highlighter).await
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
        if let Ok(mut highlighter) = Highlighter::new(&self.theme) {
            self.conversation.print_messages(&mut highlighter);
        }
        println!();
    }
}
