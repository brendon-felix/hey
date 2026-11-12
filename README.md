# hey

A minimal command-line agent with streaming and syntax highlighting. Ask a quick question, or let it
read, search and edit files and run commands in the current directory, with your approval. The focus
of `hey` is on ease of use, speed, and a pleasant terminal experience.

![demo](./hey_demo.gif)

## Features

- Any model [rig](https://rig.rs) supports: OpenAI, Anthropic, Gemini, Ollama, OpenRouter, and more
- Built-in tools to read, list, search, write, edit, move and delete files and run shell commands, with
  live command output
- [MCP](https://modelcontextprotocol.io) servers, local (stdio) or remote (streamable HTTP)
- Per-tool permissions: allow, confirm, or deny
- Conversations saved automatically and resumable with `/resume` or `hey --resume`
- Syntax highlighting via [syntect](https://github.com/trishume/syntect), with bat's themes
- A rich input editor via [reedline](https://github.com/nushell/reedline): Emacs, Vi or Helix mode,
  command completion with descriptions, a status line, history search, and composing in `$EDITOR`

## Installation

```bash
cargo install hey-rs
```

Set the API key for your model's vendor, for example:

```bash
export OPENAI_API_KEY=your_api_key_here
```

## Usage

```bash
hey                                   # interactive REPL
hey who was Ada Lovelace?             # one answer, then exit
hey -i explain this repo              # answer, then stay in the REPL
hey -r what did we decide?            # continue the most recent conversation
hey -m anthropic/claude-opus-5-5 hi   # use another model
hey -p ~/prompts/reviewer.txt         # system prompt from a file
```

### Commands

Type `/` or press Tab to see commands; the status line describes the one you are typing.

| Command | Description |
| --- | --- |
| `/help`, `/h` | Show commands and shortcuts |
| `/new`, `/reset`, `/r` | Start a new conversation |
| `/resume`, `/load`, `/l` | Continue a saved conversation |
| `/history` | Show this conversation |
| `/model [vendor/model]`, `/m` | Switch model |
| `/theme [theme]`, `/t` | Switch highlighting theme |
| `/tools` | List tools, their permissions and MCP servers |
| `/clear`, `/c` | Clear the screen |
| `/exit`, `/quit`, `/q`, `/x` | Exit |

### Keys

| Keys | Action |
| --- | --- |
| Tab | Complete commands, models and themes |
| Ctrl+R | Search input history |
| Ctrl+O | Write the message in `$EDITOR` |
| Alt+Enter | Insert a newline |
| Ctrl+C | Clear the line, or stop a running answer |
| Ctrl+D | Exit |

## Configuration

`hey` reads a TOML file, if present:

- **Linux**: `~/.config/hey/hey.toml`
- **macOS**: `~/Library/Application Support/hey/hey.toml`
- **Windows**: `%APPDATA%/hey/hey.toml`

Every option, with its default, is documented in [`defaults.toml`](./defaults.toml). An example:

```toml
model = "ollama/qwen3.8:latest"

[providers.ollama]
base_url = "https://ollama.example.com/v1"

[editor]
mode = "vi"

[tools.permissions]
shell = "allow"
"mcp:github" = "allow"

[mcp_servers.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}" }
```

### Tool permissions

Each tool call is shown as it happens. Whether it runs depends on its permission:

- `allow` runs it.
- `confirm` asks first, and offers to allow that tool for the rest of the session.
- `deny` tells the model it is not allowed.

`read`, `list` and `search` are allowed by default; everything else asks. Rules are keyed by tool
name, or `mcp:<server>:<tool>` and `mcp:<server>` for MCP tools, which the model sees as
`mcp__<server>__<tool>`.

### MCP servers

A server is local (`command`, `args`, `env`) or remote (`url`, `headers`). Entries copied from
other clients may keep their `type` key. Remote servers that only offer an OAuth sign-in are not
supported yet; most also accept an API key, for example Firecrawl:

```toml
[mcp_servers.firecrawl]
url = "https://mcp.firecrawl.dev/v2/mcp"
headers = { Authorization = "Bearer ${FIRECRAWL_API_KEY}" }
```
