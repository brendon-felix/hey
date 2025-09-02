# hey

A minimal command-line copilot with streaming and syntax highlighting

![demo](./hey_demo.gif)

## Features

- Streaming responses via [async-openai](https://github.com/64bit/async-openai)
- Extended markdown syntax highlighting via [syntect](https://github.com/trishume/syntect)
- Fully-featured input prompt via [reedline](https://github.com/nushell/reedline)
  - Vi mode support
  - Bracketed paste support (multi-line paste)
- Customizable system prompt
- Save, load and view conversation history
- Select from a list of OpenAI models
- Select from a list of builtin themes

## Installation

To install `hey`, run the following command:

```bash
cargo install hey-rs
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
```
Hey!

> ...
```

You can also send a single message as a set of arguments:

```bash
hey who was the first US president
```
```
George Washington was the first U.S. president.
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
