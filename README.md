# hey

A minimal command-line copilot with streaming and syntax highlighting

![demo](./hey_demo.gif)

## Installation

To install `hey`, run the following command:

```bash
cargo install hey
```

## Usage

Set your API key using the `OPENAI_API_KEY` environment variable:

```bash
export OPENAI_API_KEY=your_api_key_here
```

To use `hey` in REPL mode, simply run:

```bash
hey
```

You can also send a single message as a set of arguments:

```bash
hey whats up
```

To use `hey` with a custom prompt, use the `-p` flag:

```bash
hey -p ~/path/to/prompt.txt
```

### Commands

| command | description |
| --- | --- |
| `/exit`, `/quit`, `/q`, `/x` | Exit the REPL mode |
| `/clear`, `/c` | Clear the screen |
| `/reset`, `/r` | Reset the conversation |
| `/model`, `/m` | Select the model |
| `/theme`, `/t` | Select the theme |
| `/save`, `/s` | Save the conversation |
| `/load`, `/l` | Load a conversation |
| `/history` | View the conversation history |
| `/help`, `/h` | View the help menu |
