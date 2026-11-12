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
- Memory across conversations: lasting facts about you and your work are learned from answers and
  recalled when a conversation starts
- Remote hosting: run the agent and keep conversations on a server, and pick them up from any
  machine, even while an answer is in progress
- Syntax highlighting via [syntect](https://github.com/trishume/syntect), with bat's themes
- A rich input editor via [reedline](https://github.com/nushell/reedline): Emacs, Vi or Helix mode,
  command completion with descriptions, a status line, history search, and composing in `$EDITOR`
- `@` mentions that attach files, directories and saved conversations to a message

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
hey -t high prove it                  # set the thinking level (off, minimal…max)
hey -p ~/prompts/reviewer.txt         # system prompt from a file
hey --remote fermi                    # run the agent on a remote host
hey --serve                           # be a remote host
```

### Commands

Type `/` or press Tab to see commands; the status line describes the one you are typing.

| Command | Description |
| --- | --- |
| `/help`, `/h` | Show commands and shortcuts |
| `/new`, `/reset`, `/r` | Start a new conversation |
| `/resume`, `/load`, `/l` | Continue a saved conversation, following its answer if one is running |
| `/history` | Show this conversation |
| `/model [vendor/model]`, `/m` | Switch model |
| `/think [level]` | Set thinking: `off`, `minimal`…`max`, or `default` |
| `/theme [theme]`, `/t` | Switch highlighting theme |
| `/tools` | List tools, their permissions and MCP servers |
| `/memories`, `/mem` | List what hey remembers across conversations |
| `/forget` | Pick a memory to forget |
| `/clear`, `/c` | Clear the screen |
| `/exit`, `/quit`, `/q`, `/x` | Exit |

### Mentions

Type `@` to attach something to your message; a menu suggests, fuzzy-matched, files and
directories from your working directory, then `@shared:` lists the host's shared files and
`@thread:` its saved conversations.

| Mention | Attaches |
| --- | --- |
| `@src/main.rs` | The file's contents, up to 50 KiB, from your working directory |
| `@src/` | A listing of the directory, skipping what git ignores |
| `@"my notes.md"` | A path with spaces |
| `@shared:guide.md` | A file or directory from the host's shared repository, `[shared] dir` |
| `@thread:<id>` | A summary of that conversation, written by the model answering |

With a remote, plain mentions still attach from the machine you type on, while `@shared:`
and `@thread:` attach from the host, so every machine sees the same shared files. The model
gets your message as typed, followed by what it mentions. A mention that names nothing, like
`@property`, is sent as plain text. Mentions are highlighted once they name something, and
work in one-shot messages too: `hey review @src/main.rs`.

### Keys

| Keys | Action |
| --- | --- |
| Tab | Complete commands, models, themes and `@` mentions |
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
thinking = "low"

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
- `confirm` asks first, and offers to allow that tool for the rest of the conversation.
- `deny` tells the model it is not allowed.

`read`, `list` and `search` are allowed by default; everything else asks. Rules are keyed by tool
name, or `mcp:<server>:<tool>` and `mcp:<server>` for MCP tools, which the model sees as
`mcp__<server>__<tool>`.

To run every tool without asking, ignoring all rules:

```toml
[tools]
allow_all = true
```

### MCP servers

A server is local (`command`, `args`, `env`) or remote (`url`, `headers`). Entries copied from
other clients may keep their `type` key. Remote servers that only offer an OAuth sign-in are not
supported yet; most also accept an API key, for example Firecrawl:

```toml
[mcp_servers.firecrawl]
url = "https://mcp.firecrawl.dev/v2/mcp"
headers = { Authorization = "Bearer ${FIRECRAWL_API_KEY}" }
```

## Memory

`hey` remembers lasting facts about you and your work, such as your name, preferences, tools and
projects, across conversations:

- **Learning**: after each answer, the model reads the exchange and decides what is worth keeping
  and which saved memories it shows to be outdated. This takes one extra model call, made in the
  background so the prompt returns at once; a one-off `hey question` waits for it before exiting.
- **Recall**: a new conversation starts with the memories closest in meaning to its first message
  (up to `recall`), found with a vector search over their embeddings and added to the system
  prompt. They stay the same for the conversation, so the provider's prompt cache keeps working.

Embeddings come from a local [Ollama embedding model](https://ollama.com/search?c=embedding),
`qwen3-embedding:0.6b` by default (`ollama pull qwen3-embedding:0.6b`), on the daemon
`[providers.ollama]` names, else `OLLAMA_API_BASE_URL`'s, else `localhost:11434`. Each memory is
embedded once, the first time a search needs it. Until there are more memories than `recall`,
every conversation gets all of them and nothing is embedded; if the model cannot be reached, a
conversation starts with the newest and says why.

Memories are kept as a Markdown list (`memories.md` in hey's data directory) that you can read and
edit, with their embeddings beside it in `memories.vectors.json`; `/memories` lists them and
`/forget` removes one. With a remote, they are the server's.

```toml
[memory]
embedding_model = "ollama/embeddinggemma"   # another embedding model
model = "openai/gpt-5.4-mini"               # a cheaper model to decide what to remember
recall = 20
# enabled = false                           # turn memory off
```

## Remote hosting

`hey` can run the agent on another machine, such as a home server, which keeps the conversations.
Start a conversation on your laptop, close it, and continue it from your desktop. A running answer
carries on without a client. Opening the conversation elsewhere replays the answer so far, then
follows it live. Tool calls that need confirming wait until any client answers them.

On the server, set the address and run `hey --serve` in the directory the agent should work in:

```toml
[server]
listen = "0.0.0.0:7700"
```

The first start generates a token and says where it saved it, `hey/server-token` in the data
directory (`~/.local/share` on Linux). Clients need that token.

On each client, name the server and make it the default:

```toml
remote = "fermi"

[remotes.fermi]
url = "http://fermi:7700"
```

The first time a client connects, it asks for the token and keeps it in the system keychain. Where
there is none, as on a server without a desktop session, it keeps it in a private file in the data
directory.

To manage the token yourself, set `token_file` on both sides, or `token`, which may name
environment variables. A `token_file` in a folder synced between your machines needs no copying:
the server generates the token there, and clients read it from their copy.

```toml
[server]
token_file = "~/Sync/hey-token"    # or token = "${HEY_TOKEN}"

[remotes.fermi]
url = "http://fermi:7700"
token_file = "~/Sync/hey-token"
```

With a remote, everything about the agent comes from the server's config and environment: model
API keys, tools and their permissions, MCP servers and where conversations are saved. The client
chooses the model and thinking level per run (`/model`, `/think`) and keeps its own display and
editor settings. `hey --local` runs the agent on the client instead.

The server runs tools for anyone holding the token and does not use TLS. Reach it over a private
network such as Tailscale, WireGuard or an SSH tunnel, or behind an HTTPS reverse proxy.

A run lives in the server's memory until it finishes and is saved. Restarting the server loses an
answer in progress, but not the conversation before it.
