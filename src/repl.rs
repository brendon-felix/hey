/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use yansi::Paint;

use crate::commands::Command;
use crate::conversation::Conversation;
use crate::editor::{Editor, Input};
use crate::render::{Highlighter, snailprint};
use crate::response::{create_request, generate_title, stream_response};
use crate::utils::{
    clear_console, print_sample_text, print_separator, select_filename, select_json_file,
    select_model, select_theme,
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
        let _ = snailprint("\nHey!\n\n", 10000);
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Command(command) => match command {
                    Command::Exit => {
                        let _ = snailprint(&format!("\n{}\n\n", "Exiting...".red()), 5000);
                        sleep(Duration::from_millis(250));
                        return Ok(());
                    }
                    Command::Clear => {
                        let _ = snailprint(&format!("\n{}\n\n", "Clearing...".yellow()), 5000);
                        sleep(Duration::from_millis(250));
                        clear_console();
                    }
                    Command::Reset => {
                        let _ = snailprint(
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
                        let selection = match select_model(&self.model) {
                            Err(e) => {
                                let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                                continue;
                            }
                            Ok(model) => model,
                        };
                        self.model = selection;
                    }
                    Command::SelectTheme => {
                        let selection = match select_theme() {
                            Err(e) => {
                                let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                                continue;
                            }
                            Ok(theme) => theme,
                        };
                        self.theme = selection;
                        if let Err(e) = print_sample_text(&self.theme) {
                            let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                        }
                    }
                    Command::Save => match self.save_conversation().await {
                        Err(e) => {
                            let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                            continue;
                        }
                        Ok(_) => {}
                    },
                    Command::Load => match self.load_conversation() {
                        Err(e) => {
                            let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                            continue;
                        }
                        Ok(_) => {
                            print_separator();
                            self.print_conversation();
                        }
                    },
                    Command::History => {
                        self.print_conversation();
                    }
                    Command::Help => {
                        print_help();
                    }
                    Command::Invalid => {
                        let _ = snailprint(
                            &format!(
                                "\nInvalid command. Type /{} for a list of commands.\n\n",
                                "help".cyan()
                            ),
                            2000,
                        );
                    }
                },
                Input::Message(message) => {
                    self.conversation.add_user_message(message);
                    let response = match self.get_response().await {
                        Err(e) => {
                            let _ = snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                            continue;
                        }
                        Ok(response) => response,
                    };
                    self.conversation.add_assistant_message(response);
                }
                Input::Invalid => {}
            }
        }
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
        let _ = snailprint(
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
            let _ = snailprint("\nNo conversation history available.\n\n", 5000);
            return;
        }
        if let Ok(mut highlighter) = Highlighter::new(&self.theme) {
            self.conversation.print_messages(&mut highlighter);
        }
        println!();
    }
}

fn print_help() {
    let _ = snailprint(&format!("\n{}\n", "Available commands:".blue()), 1000);
    // snailprint("TODO\n", 10000);
    enum_iterator::all::<Command>().for_each(|command| {
        let _ = snailprint(
            &format!(
                "{}\n",
                command
                    .strings()
                    .iter()
                    .map(|s| format!("/{}", s.cyan()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            1000,
        );
    });
}
