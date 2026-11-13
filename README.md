# hey

A minimal command-line agent with streaming and syntax highlighting. Ask a quick question, or let it
read, search and edit files and run commands in the current directory, with your approval. The focus
of `hey` is on ease of use, speed, and a pleasant terminal experience.

![demo](./hey_demo.gif)

## Features

- Any model [rig](https://rig.rs) supports: OpenAI, Anthropic, Gemini, Ollama, OpenRouter, and more
- Built-in tools to read, list, search, write, edit, move and delete files and run shell commands, with
  live command output
- Web tools: `fetch` reads a page as Markdown in a fraction of a second, and `web_search` searches
  with Brave
- [MCP](https://modelcontextprotocol.io) servers, local (stdio) or remote (streamable HTTP)
- [Agent Skills](https://agentskills.io): instructions for particular tasks that the model loads
  when a task calls for one, or that you invoke with `/name`
- Per-tool permissions: allow, confirm, or deny
- Subagents that work on tasks in parallel, each in a directory you let it into, on the host or
  on your own machine
- Conversations saved automatically, with a list that shows which are answering and which have an
  answer you have not seen, to open, archive, delete or fork them
- Memory across conversations: lasting facts about you and your work are learned from answers and
  recalled when a conversation starts
- Remote hosting: run the agent and keep conversations on a server, and pick them up from any
  machine, even while an answer is in progress
- Syntax highlighting via [syntect](https://github.com/trishume/syntect), with bat's themes
- A rich input editor via [reedline](https://github.com/nushell/reedline): Emacs, Vi or Helix mode,
  command completion with descriptions, a status line, history search, and composing in `$EDITOR`
- `@` mentions that attach files, directories, saved conversations and skills to a message

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
hey -l                                # list conversations to open, archive, delete or fork
hey --memories                        # list memories to write, edit or forget
hey -m anthropic/claude-opus-5-5 hi   # use another model
hey -t high prove it                  # set the thinking level (off, minimal…max)
hey -p ~/prompts/reviewer.txt         # system prompt from a file
hey --remote fermi                    # run the agent on a remote host
hey --serve                           # be a remote host
```

### Commands

Type `/` or press Tab to see commands and skills; the status line describes the one you are typing.

| Command | Description |
| --- | --- |
| `/help`, `/h` | Show commands and shortcuts |
| `/new`, `/reset`, `/r` | Start a new conversation |
| `/threads`, `/resume`, `/load`, `/l` | List conversations to open, archive, delete or fork |
| `/fork` | Continue in a copy of this conversation, leaving the original as it is |
| `/history` | Show this conversation |
| `/copy` | Copy the latest reply as Markdown |
| `/copy-all` | Copy this conversation as Markdown |
| `/model [vendor/model]`, `/m` | Switch model |
| `/think [level]` | Set thinking: `off`, `minimal`…`max`, or `default` |
| `/theme [theme]`, `/t` | Switch theme; without one, pick with a live preview |
| `/tools` | List tools, their permissions and MCP servers |
| `/skills` | List skills and why any did not load |
| `/<skill> [message]` | Invoke a skill, with the rest of the line as your message |
| `/memories`, `/mem` | List memories to write, edit or forget |
| `/clear`, `/c` | Clear the screen |
| `/exit`, `/quit`, `/q`, `/x` | Exit |

`/model`, `/think` and `/theme` save their choice to the config file, so later sessions start
with it; `--model` and `--think` apply to one run only.

`/copy` and `/copy-all` copy the Markdown the model wrote, not how it shows: without the
terminal's line wrapping, reasoning or tool calls. `/copy` takes the latest reply, all the model
wrote in answer to one message; `/copy-all` heads each message `## User` or `## Assistant`, leaving out
what mentions attached. Over SSH, or where there is no system clipboard, they ask the terminal
to set its machine's clipboard (OSC 52), which most modern terminals do.

### Mentions

Type `@` to attach something to your message; a menu suggests, fuzzy-matched, files and
directories from your working directory, then `@shared:` lists the host's shared files,
`@thread:` its saved conversations and `@skill:` its skills.

| Mention | Attaches |
| --- | --- |
| `@src/main.rs` | The file's contents, up to 50 KiB, from your working directory |
| `@src/` | A listing of the directory, skipping what git ignores |
| `@"my notes.md"` | A path with spaces |
| `@shared:guide.md` | A file or directory from the host's shared repository, `[shared] dir` |
| `@thread:<id>` | A summary of that conversation, written by the model answering |
| `@skill:<name>` | The skill's instructions, as `/<name>` does, anywhere in the message |

With a remote, plain mentions still attach from the machine you type on, while `@shared:`,
`@thread:` and `@skill:` attach from the host, so every machine sees the same shared files. The model
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
| Ctrl+C | Stop a running answer, or exit |
| Ctrl+D | Exit |

### Conversations

`hey -l` or `/threads` lists conversations, newest first, with what each is doing:

| Mark | Meaning |
| --- | --- |
| spinner | Answering, whether or not any terminal is following it |
| `?` | Waiting for a tool call to be approved, or for your machine to take up a subagent |
| `●` | Finished an answer you have not seen yet |

An answer carries on if you close the terminal that asked; its conversation shows `●` until you
open it. Opening a conversation that is still answering follows the answer live.

| Keys | Action |
| --- | --- |
| ↑ ↓, `j` `k`, `g` `G` | Move |
| Enter | Open the conversation |
| `a` | Archive it, or restore it among the archived |
| `d` | Delete it, after you confirm with `y` |
| `f` | Fork it: a copy to continue apart |
| Tab | Switch between the conversations and the archived ones |
| `/` | Filter by title; Esc clears it |
| `q`, Esc | Close the list |

With a remote, the list is the host's, so every machine sees the same conversations and marks.
`hey -r` skips archived conversations.

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

`read`, `list`, `search`, `skill` and `spawn_agents` are allowed by default; everything else asks. Rules are
keyed by tool name, or `mcp:<server>:<tool>` and `mcp:<server>` for MCP tools, which the model sees
as `mcp__<server>__<tool>`.

To run every tool without asking, ignoring all rules:

```toml
[tools]
allow_all = true
```

### Subagents

The agent can hand tasks to subagents with its `spawn_agents` tool, several at once. Each works
on one task in one directory, with the built-in tools, and replies with a report, which is all
the agent sees; its tool calls show under its label as it works, and its own tool calls are
permitted as yours are. The agent can follow up with a subagent later in the conversation, which
remembers its earlier work until hey (or, with a remote, the machine it ran on) restarts.
Subagents cannot start subagents.

With a remote, a subagent works on the host or on the machine you type on, as the agent
chooses. One on your machine runs in your `hey`, with your config, model keys and tool
permissions, and shows in every terminal following the conversation. It waits, marked `?`,
until a terminal on your machine follows the conversation.

Before a subagent works in a directory, the machine it runs on checks its rules, keyed by its
name (`hostname -s`, or `HEY_MACHINE`), so one config can serve every machine:

```toml
[subagents.machines.fermi]
allow = ["~/Projects"]
exclude = ["~/Projects/secrets", "~/.ssh"]
```

A directory inside one in `allow` is used at once. One inside `exclude` is refused, and a
subagent's file tools refuse anything in an excluded directory, even within its own. For any
other, you are asked: **No**, **Yes** for this conversation, or **Always**, which adds it to
`allow` in that machine's config. `tools.allow_all` does not skip the question. A subagent's
file tools stay inside its directory; its shell commands start there, but what they do is not
confined, so keep `shell` on `confirm` if that matters. `[subagents] model` picks a cheaper
model for them, and `enabled = false` turns them off.

### Web

`fetch` downloads a page and gives the model its main content as Markdown, found as Firefox's
Reader View finds it, or the whole page when asked. It takes about as long as the download, but
does not run JavaScript, so pages built by their scripts come back nearly empty; an MCP server
such as Firecrawl's can render those.

`web_search` is offered when `BRAVE_API_KEY` is set to a
[Brave Search API](https://brave.com/search/api/) key. It uses Brave's LLM Context endpoint, which
answers with passages already taken from the pages it found, so one call often does without a
`fetch`. Brave includes $5 of searches a month, about 1,000.

Both ask before each call unless allowed:

```toml
[tools.permissions]
fetch = "allow"
web_search = "allow"
```

### MCP servers

A server is local (`command`, `args`, `env`) or remote (`url`, `headers`). Entries copied from
other clients may keep their `type` key. Remote servers that only offer an OAuth sign-in are not
supported yet; most also accept an API key, for example Firecrawl:

```toml
[mcp_servers.firecrawl]
url = "https://mcp.firecrawl.dev/v2/mcp"
headers = { Authorization = "Bearer ${FIRECRAWL_API_KEY}" }
tools = ["firecrawl_search", "firecrawl_scrape"]
```

Every tool a server offers is described to the model on every call; Firecrawl's 36 come to about
21,000 tokens. `tools` offers only those named. A result longer than `[tools] result_bytes`
(16 KiB) is cut, and kept whole in a file the model can `read` or `search`; so is a `fetch` or
`web_search` result.

## Skills

A skill is a folder of instructions for a particular task, in the
[Agent Skills](https://agentskills.io/specification) format other agents share: a `SKILL.md`
with a `name` and `description`, and any scripts, references or templates it refers to.

```markdown
---
name: release
description: Cut a release of this project. Use when asked to release or publish a version.
---

1. Bump the version in Cargo.toml…
```

`hey` finds skills where the agent works:

- `~/.agents/skills/<name>/SKILL.md`, for every project
- `.agents/skills/<name>/SKILL.md` in the working directory and each directory above it up to
  the repository root; the nearer of two skills with one name wins, so a project can replace
  a global skill

The model sees every skill's name and description, and loads one's instructions with its
`skill` tool when a task matches; it reads the files a skill refers to as it needs them. You
invoke one yourself with `/<name>`, the rest of the line being your message, or `@skill:<name>`
anywhere in a message; built-in commands win over skills of the same name. A skill with
`disable-model-invocation: true` is left out of what the model sees, so only you start it.

Skills are read afresh for every answer, so adding or editing one takes effect at once. One
whose `SKILL.md` breaks the spec, such as a `name` that is not lowercase-hyphenated or differs
from its folder's, is not loaded; `/skills` lists the skills and why any did not load. With a
remote, skills are the host's, from where `hey --serve` runs; a subagent has the skills of the
machine and directory it works in.

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
edit, with their embeddings beside it in `memories.vectors.json`. With a remote, they are the
server's.

`hey --memories` or `/memories` lists them, newest first:

| Keys | Action |
| --- | --- |
| ↑ ↓, `j` `k`, `g` `G` | Move |
| Enter, `e` | Edit the memory in place; Enter saves it, Esc leaves it as it was |
| `n` | Write a new memory |
| `d` | Forget it, after you confirm with `y` |
| `/` | Filter; Esc clears it |
| `q`, Esc | Close the list |

While editing, ← →, Home and End move, Ctrl+W erases a word and Ctrl+U everything before the
cursor. A memory hey forgets while you edit it is saved anew with your words.

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

Every terminal showing a conversation shows each answer in it as it streams, whichever terminal
asked; what you were typing waits until the answer ends. Several conversations can answer at
once. The status at the right of the prompt counts the others: a spinner for those answering, and
a blue dot for those that finished while no terminal was showing them. `/threads` marks them the
same way.

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
chooses the model and thinking level per run (`/model`, `/think`, which then last only the session,
since the server's config sets the defaults) and keeps its own display and editor settings.
`hey --local` runs the agent on the client instead.

The model is told that its tools act on the server, and which machine and directory you are on,
so it does not look for your files on the server; mention them with `@` to attach them.

The server runs tools for anyone holding the token and does not use TLS. Reach it over a private
network such as Tailscale, WireGuard or an SSH tunnel, or behind an HTTPS reverse proxy.

A proxy that wants its own credentials, such as Cloudflare Access in front of a Cloudflare Tunnel,
gets them from `headers`, sent with every request. Like `token`, they may name environment
variables. A quiet stream carries a blank line every 30 seconds, so proxies keep it open through
long tool calls.

```toml
[remotes.fermi]
url = "https://hey.example.com"
headers = { CF-Access-Client-Id = "${HEY_ACCESS_ID}", CF-Access-Client-Secret = "${HEY_ACCESS_SECRET}" }
```

A run lives in the server's memory until it finishes and is saved. Restarting the server loses an
answer in progress, but not the conversation before it.
