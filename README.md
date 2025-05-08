# Rusty GPT

Rusty GPT is a command-line application built in Rust that interacts with OpenAI's GPT models. It allows users to send messages to the model, receive responses, and manage conversation history. The application supports both interactive conversations and single-message queries.

## Installation

1. Clone the repository:
   ```bash
   git clone https://github.com/your-username/rusty-gpt.git
   cd rusty-gpt
   ```

2. Build the project:
   ```bash
   cargo build --release
   ```

3. Ensure the following files exist in the project directory:
   - `api_key.txt`: Contains your OpenAI API key.
   - `system_prompt.txt`: Contains the system prompt for the GPT model.

## Usage

Run the application with the following command:
```bash
cargo run --release -- [OPTIONS] [MESSAGE]
```

### Options

- `-a, --api-key <FILE>`: Path to the API key file (default: `api_key.txt`).
- `-p, --prompt-path <FILE>`: Path to the system prompt file (default: `system_prompt.txt`).

### Examples

1. Start an interactive conversation:
   ```bash
   cargo run --release
   ```

2. Send a single message to the model:
   ```bash
   cargo run --release -- "Hello, how are you?"
   ```

3. Specify a custom API key file and system prompt file:
   ```bash
   cargo run --release -- -a custom_api_key.txt -p custom_prompt.txt
   ```

## Commands in Interactive Mode

- `exit` or `/q`: Exit the application.
- `clear` or `/c`: Clear the conversation history.
- `history` or `/h`: View the conversation history.
- `prompt` or `/p`: View the system prompt.
- `help` or `?`: Display help information.

## Dependencies

- `chatgpt_rs`: For interacting with OpenAI's GPT models.
- `tokio`: For asynchronous runtime.
- `futures-util`: For working with streams.
- `clap`: For command-line argument parsing.
- `rand`: For random number generation.
