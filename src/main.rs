// TODO:
// - Add flags for
//   - Providing API key .txt file
//   - Providing system prompt .txt file
//   - Providing model name
//   - Providing temperature
//   - Providing max tokens
// - Create utils.rs to hold utility functions
// - Fix timeout issue

use std::fs::File;
use std::io::Read;
use chatgpt::prelude::*;
use clap::Parser as ArgParser;

mod commands;
mod conversation;
use conversation::{stream_single_response, start_conversation};

#[derive(ArgParser, Debug)]
struct Args {
    /// Path to the API key file
    #[arg(long, short, default_value = "api_key.txt")]
    api_key: String,

    /// Path to the system prompt file
    #[arg(long, short, default_value = "system_prompt.txt")]
    prompt_path: String,

    /// Single message to send to the model
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let api_key: String = File::open(args.api_key)
        .and_then(|mut file| {
            let mut key = String::new();
            file.read_to_string(&mut key)?;
            Ok(key)
        })
        .unwrap_or_else(|_| {
            eprintln!("Failed to read API key from file. Please ensure 'api_key.txt' exists.");
            std::process::exit(1);
        });

    let config = ModelConfiguration {
        engine: ChatGPTEngine::Gpt35Turbo,
        ..Default::default()
    };
    let client = ChatGPT::new_with_config(api_key, config)?;

    if args.args.is_empty() {
        // If no message is provided, start a conversation
        start_conversation(&client, args.prompt_path).await?;
    } else {
        // If a single message is provided, send it to the model
        let message = args.args.join(" ");
        stream_single_response(&client, message, args.prompt_path).await?;
    }
    Ok(())
}





// use tokio;

// #[tokio::main]
// async fn main() {
//     let start_time = std::time::Instant::now(); // t0
//     let other_task = tokio::spawn(async {
//         loop_task().await; // Call the async function
//     });
//     let task1 = async_task1();
//     println!("t1 = {}s", start_time.elapsed().as_secs()); // t1
//     let task2 = async_task2();
//     println!("t2 = {}s", start_time.elapsed().as_secs()); // t2
//     task1.await;
//     println!("t3 = {}s", start_time.elapsed().as_secs()); // t3
//     task2.await;
//     println!("t4 = {}s", start_time.elapsed().as_secs()); // t4
//     blocking_task();
//     println!("t5 = {}s", start_time.elapsed().as_secs()); // t5
//     other_task.await.unwrap(); // Wait for the other task to finish
// }

// async fn async_task1() {
//     println!("Starting async_task1...");
//     tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
//     println!("async_task1 completed");
// }

// async fn async_task2() {
//     println!("Starting async_task1...");
//     tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
//     println!("async_task1 completed");
// }

// fn blocking_task() {
//     println!("Starting blocking_task...");
//     std::thread::sleep(std::time::Duration::from_secs(1));
//     println!("blocking_task completed");
// }

// async fn loop_task() {
//     for i in 1..=30 {
//         println!("Other task is working... {}", i);
//         tokio::time::sleep(tokio::time::Duration::from_millis(250)).await; // Simulate a delay
//     }
// }