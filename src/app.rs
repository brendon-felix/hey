/* -------------------------------------------------------------------------- */
/*                                   app.rs                                   */
/* -------------------------------------------------------------------------- */

use std::thread::sleep;
use std::time::Duration;

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use yansi::Paint;

use crate::commands::Command;
use crate::conversation::Conversation;
use crate::editor::{Editor, Input};
use crate::render::{Highlighter, snailprint};
use crate::response::{create_request, generate_title, stream_response};
use crate::utils::{
    clear_console, print_separator, select_filename, select_json_file, select_model,
};

pub struct App {
    client: Client<OpenAIConfig>,
    model: String,
    conversation: Conversation,
    editor: Editor,
    history_file: Option<String>,
}

impl App {
    pub fn new(system_prompt: String) -> Self {
        let client = Client::new();
        let model = "gpt-4o".to_string();
        let conversation = Conversation::new(system_prompt);
        let editor = Editor::new();
        let history_file = None;
        App {
            client,
            model,
            conversation,
            editor,
            history_file,
        }
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        snailprint("\nHey!\n\n", 10000);
        loop {
            let input = self.editor.get_input();
            match input {
                Input::Command(command) => match command {
                    Command::Exit => {
                        snailprint(&format!("\n{}\n\n", "Exiting...".red()), 5000);
                        sleep(Duration::from_millis(250));
                        return Ok(());
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
                        print_separator();
                        println!();
                        sleep(Duration::from_millis(500));
                        self.conversation.reset();
                    }
                    Command::SelectModel => {
                        let selection = select_model(&self.model)?;
                        self.model = selection;
                        snailprint(
                            &format!("\n{} {}\n\n", "Model changed to".green(), self.model.blue()),
                            2000,
                        );
                    }
                    Command::Save => {
                        self.save_conversation().await?;
                    }
                    Command::Load => match self.load_conversation() {
                        Err(e) => {
                            snailprint(&format!("\n{} {}\n\n", "Error:".red(), e), 5000);
                            continue;
                        }
                        Ok(_) => {
                            print_separator();
                            println!();
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
                        snailprint(
                            &format!(
                                "\nInvalid command. Type /{} for a list of commands.\n\n",
                                "help".cyan()
                            ),
                            2000,
                        );
                    }
                },
                Input::Message(message) => {
                    // self.push_user_message(&message);
                    self.conversation.add_user_message(message);
                    let response = self.get_response().await?;
                    // dbg!(&response);
                    // self.push_assistant_message(&response);
                    self.conversation.add_assistant_message(response);
                }
                Input::Invalid => {}
            }
        }
    }

    pub async fn get_response(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let request = create_request(&self.model, 2048u32, self.conversation.messages.clone())?;
        let highlighter = &mut Highlighter::new();
        stream_response(&self.client, request, highlighter).await
    }

    async fn save_conversation(&self) -> Result<(), Box<dyn std::error::Error>> {
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

    fn load_conversation(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let conversations_folder = "./";

        let filename = match select_json_file(conversations_folder) {
            Ok(None) => {
                // snailprint("\nLoad cancelled.\n\n", 2000);
                // return Ok(());
                return Err("Load cancelled.".into());
            }
            Ok(Some(filename)) => filename,
            Err(e) => {
                return Err(e.into());
            }
        };
        let filepath = filename; // need to add config setting for conversations folder
        self.history_file = Some(filepath.clone());
        self.conversation = Conversation::from_json_file(&filepath)?;
        snailprint(
            &format!("\nConversation loaded successfully from {}.\n", filepath),
            2000,
        );
        Ok(())
    }

    fn print_conversation(&self) {
        if self.conversation.messages.len() <= 1 {
            snailprint("\nNo conversation history available.\n\n", 5000);
            return;
        }
        self.conversation.print_messages();
        println!();
    }
}

fn print_help() {
    snailprint(&format!("\n{}\n", "Available commands:".blue()), 1000);
    // snailprint("TODO\n", 10000);
    enum_iterator::all::<Command>().for_each(|command| {
        snailprint(
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
