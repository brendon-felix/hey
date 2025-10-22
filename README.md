# hey

A minimal command-line application for quickly interacting with OpenAI's chat models, with support for streaming and syntax highlighting. The focus of `hey` is on ease of use, speed, and a pleasant terminal experience.

![demo](./hey_demo.gif)

## Features

- Streaming responses via [async-openai](https://github.com/64bit/async-openai)
- Syntax highlighting via [syntect](https://github.com/trishume/syntect)
- Rich input editor via [reedline](https://github.com/nushell/reedline) with Vi mode, multi-line paste, and persistent history
- Conversation history - quickly save, load, and view past conversations
- Customizable - Vi mode, themes, and more

## Installation

### Prerequisites

- [Rust](https://rustup.rs/) (for installation via cargo)
- OpenAI API key

### Install

```bash
cargo install hey-rs
```

### Setup

Set your OpenAI API key:

```bash
export OPENAI_API_KEY=your_api_key_here
```

## Usage

### Interactive Mode (REPL)

```bash
hey
```

### Single Message

```bash
hey who was the first US president
```

### With Custom Prompt File

```bash
hey -p ~/path/to/prompt.txt
```

## Commands

| Command | Description |
| --- | --- |
| `/exit`, `/quit`, `/q`, `/x` | Exit the REPL |
| `/clear`, `/c` | Clear screen |
| `/reset`, `/r` | Reset conversation |
| `/model`, `/m` | Select model |
| `/theme`, `/t` | Select theme |
| `/save`, `/s` | Save conversation |
| `/load`, `/l` | Load conversation |
| `/history` | View conversation history |
| `/help`, `/h` | Show help |

## Configuration

`hey` uses a TOML configuration file located at:

- **Linux**: `~/.config/hey/hey.toml`
- **macOS**: `~/Library/Application Support/hey/hey.toml`
- **Windows**: `%APPDATA%/hey/hey.toml`

### Common Options

```toml
# AI Model settings
model = "gpt-4o"                    # or "gpt-4o-mini", "gpt-3.5-turbo"
system_prompt = "You are a helpful coding assistant."
max_tokens = 2048

# Display
syntax_highlighting = true
theme = "ansi"                      # Run /theme to see all options
wrap_width = 100                    # 0 to disable wrapping
ansi_colors = true
animations = true                   # Typewriter effect

# Editor
edit_mode = "emacs"                 # or "vi"
bracketed_paste = true
reedline_history = true             # Save input history
history_max_size = 1000             # Max number of inputs to save

# Files
conversations_folder = "~/.hey"     # Where to save conversations
```

### All Configuration Options

| Option | Default | Description |
| --- | --- | --- |
| `system_prompt` | `"You are a helpful assistant."` | Initial context for AI |
| `model` | `"gpt-4o"` | OpenAI model to use |
| `max_tokens` | `2048` | Response length limit |
| `enter_repl` | `false` | Force REPL mode with CLI message |
| `wrap_width` | `100` | Text wrapping width (0 = disabled) |
| `syntax_highlighting` | `true` | Code syntax highlighting |
| `theme` | `"ansi"` | Highlighting color scheme |
| `ansi_colors` | `true` | Colored terminal output |
| `animations` | `true` | Typewriter text effect |
| `edit_mode` | `"emacs"` | Input editor mode ("emacs" or "vi") |
| `bracketed_paste` | `true` | Multi-line paste support |
| `conversations_folder` | `"./"` | Directory for saved conversations |
| `reedline_history` | `true` | Persist input history across sessions |
| `history_max_size` | `1000` | Maximum input history size |

See [`defaults.toml`](./defaults.toml) for detailed documentation of all options.

### Example Configurations

**Minimal setup:**
```toml
model = "gpt-4o-mini"
system_prompt = "You are a helpful coding assistant."
```

**Plain text output:**
```toml
syntax_highlighting = false
ansi_colors = false
wrap_width = 0
```

**Vi user setup:**
```toml
edit_mode = "vi"
theme = "base16"
conversations_folder = "~/Documents/hey-conversations"
```
