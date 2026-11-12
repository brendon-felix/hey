/* -------------------------------------------------------------------------- */
/*                                 config.rs                                  */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

use crate::permissions::{self, Permission};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub thinking: Option<Thinking>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u64>,
    pub max_turns: usize,
    pub greetings: bool,
    pub providers: Providers,
    pub display: Display,
    pub editor: Editor,
    pub sessions: Sessions,
    pub shared: Shared,
    pub memory: Memory,
    pub tools: Tools,
    pub mcp_servers: BTreeMap<String, McpServer>,
    pub remote: Option<String>,
    pub remotes: BTreeMap<String, Remote>,
    pub server: Server,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "openai/gpt-5.5".into(),
            thinking: None,
            system_prompt: None,
            max_tokens: None,
            max_turns: 50,
            greetings: true,
            providers: Providers::default(),
            display: Display::default(),
            editor: Editor::default(),
            sessions: Sessions::default(),
            shared: Shared::default(),
            memory: Memory::default(),
            tools: Tools::default(),
            mcp_servers: BTreeMap::new(),
            remote: None,
            remotes: BTreeMap::new(),
            server: Server::default(),
        }
    }
}

impl Config {
    /// The config at `path`, else the default file if it exists, else the defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => match config_dir().map(|dir| dir.join("hey.toml")) {
                Some(path) if path.exists() => path,
                _ => return Ok(Self::default()),
            },
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
    }
}

/// How much the model reasons before answering. Unset leaves it to the
/// model's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Thinking {
    pub const ALL: [Thinking; 7] = [
        Thinking::Off,
        Thinking::Minimal,
        Thinking::Low,
        Thinking::Medium,
        Thinking::High,
        Thinking::Xhigh,
        Thinking::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Thinking::Off => "off",
            Thinking::Minimal => "minimal",
            Thinking::Low => "low",
            Thinking::Medium => "medium",
            Thinking::High => "high",
            Thinking::Xhigh => "xhigh",
            Thinking::Max => "max",
        }
    }
}

impl std::str::FromStr for Thinking {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(text))
            .ok_or_else(|| {
                let levels = Self::ALL.map(Thinking::as_str).join(", ");
                format!("unknown thinking level `{text}`; use one of {levels}")
            })
    }
}

impl From<Thinking> for rig_core::completion::Reasoning {
    fn from(thinking: Thinking) -> Self {
        use rig_core::completion::{Effort, Reasoning};
        match thinking {
            Thinking::Off => Reasoning::Off,
            Thinking::Minimal => Reasoning::Effort(Effort::Minimal),
            Thinking::Low => Reasoning::Effort(Effort::Low),
            Thinking::Medium => Reasoning::Effort(Effort::Medium),
            Thinking::High => Reasoning::Effort(Effort::High),
            Thinking::Xhigh => Reasoning::Effort(Effort::XHigh),
            Thinking::Max => Reasoning::Effort(Effort::Max),
        }
    }
}

/// Settings for a vendor, by the name model references start with.
pub type Providers = BTreeMap<String, Provider>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provider {
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    pub syntax_highlighting: bool,
    pub theme: String,
    pub wrap_width: usize,
    pub colors: bool,
    pub animations: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            syntax_highlighting: true,
            theme: "ansi".into(),
            wrap_width: 100,
            colors: true,
            animations: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    Emacs,
    Vi,
    Helix,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Editor {
    pub mode: EditMode,
    pub bracketed_paste: bool,
    pub history: bool,
    pub history_size: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            mode: EditMode::Emacs,
            bracketed_paste: true,
            history: true,
            history_size: 1000,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sessions {
    dir: Option<String>,
}

impl Sessions {
    pub fn dir(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => expand_path(dir),
            None => data_dir()
                .map(|dir| dir.join("sessions"))
                .context("set `sessions.dir`"),
        }
    }
}

/// The host's repository of files any client can attach with `@shared:`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shared {
    dir: Option<String>,
}

impl Shared {
    pub fn dir(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => expand_path(dir),
            None => data_dir()
                .map(|dir| dir.join("shared"))
                .context("set `shared.dir`"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub enabled: bool,
    /// The model that decides what to remember; else the conversation's.
    pub model: Option<String>,
    /// The model that places memories by meaning, to recall the closest.
    pub embedding_model: String,
    /// Most memories a conversation starts with.
    pub recall: usize,
    file: Option<String>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            embedding_model: "ollama/qwen3-embedding:0.6b".into(),
            recall: 20,
            file: None,
        }
    }
}

impl Memory {
    pub fn file(&self) -> Result<PathBuf> {
        match &self.file {
            Some(file) => expand_path(file),
            None => data_dir()
                .map(|dir| dir.join("memories.md"))
                .context("set `memory.file`"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tools {
    /// Run every tool without asking, whatever `default` and `permissions` say.
    pub allow_all: bool,
    pub default: Permission,
    #[serde(deserialize_with = "over_default_rules")]
    pub permissions: BTreeMap<String, Permission>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            allow_all: false,
            default: Permission::Confirm,
            permissions: permissions::default_rules(),
        }
    }
}

/// The user's rules extend the defaults rather than replace them, so adding
/// `shell = "allow"` keeps the read-only tools allowed.
fn over_default_rules<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Permission>, D::Error> {
    let mut rules = permissions::default_rules();
    rules.extend(BTreeMap::deserialize(deserializer)?);
    Ok(rules)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawMcpServer")]
pub enum McpServer {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    /// Redundant with `command`/`url`, but accepted so entries copied from
    /// other clients' docs work as written.
    #[serde(rename = "type")]
    transport: Option<Transport>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Transport {
    #[serde(alias = "local")]
    Stdio,
    #[serde(alias = "remote", alias = "streamable-http", alias = "streamableHttp")]
    Http,
}

impl TryFrom<RawMcpServer> for McpServer {
    type Error = &'static str;

    fn try_from(raw: RawMcpServer) -> Result<Self, Self::Error> {
        let local_only = !raw.args.is_empty() || !raw.env.is_empty();
        let server = match (raw.command, raw.url) {
            (Some(_), Some(_)) => Err("set either `command` or `url`, not both"),
            (None, None) => Err("set `command` for a local server or `url` for a remote one"),
            (Some(_), None) if !raw.headers.is_empty() => {
                Err("`headers` only applies to a remote server (`url`)")
            }
            (None, Some(_)) if local_only => {
                Err("`args` and `env` only apply to a local server (`command`)")
            }
            (Some(command), None) => Ok(Self::Stdio {
                command,
                args: raw.args,
                env: raw.env,
            }),
            (None, Some(url)) => Ok(Self::Http {
                url,
                headers: raw.headers,
            }),
        }?;
        match (raw.transport, &server) {
            (Some(Transport::Stdio), Self::Http { .. }) => {
                Err("a local (`stdio`) server is started with `command`, not `url`")
            }
            (Some(Transport::Http), Self::Stdio { .. }) => {
                Err("a remote (`http`) server is reached at a `url`, not a `command`")
            }
            _ => Ok(server),
        }
    }
}

/// A host that runs the agent and keeps its threads, reached by `hey --remote`.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawRemote")]
pub struct Remote {
    pub url: String,
    pub token: TokenSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemote {
    url: String,
    token: Option<String>,
    token_file: Option<String>,
}

impl TryFrom<RawRemote> for Remote {
    type Error = &'static str;

    fn try_from(raw: RawRemote) -> Result<Self, Self::Error> {
        Ok(Self {
            url: raw.url,
            token: TokenSource::new(raw.token, raw.token_file)?,
        })
    }
}

const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7700);

/// How `hey --serve` hosts the agent for remote clients.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawServer")]
pub struct Server {
    pub listen: SocketAddr,
    pub token: TokenSource,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN,
            token: TokenSource::Kept,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawServer {
    listen: SocketAddr,
    token: Option<String>,
    token_file: Option<String>,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN,
            token: None,
            token_file: None,
        }
    }
}

impl TryFrom<RawServer> for Server {
    type Error = &'static str;

    fn try_from(raw: RawServer) -> Result<Self, Self::Error> {
        Ok(Self {
            listen: raw.listen,
            token: TokenSource::new(raw.token, raw.token_file)?,
        })
    }
}

/// Where the token a host and its clients share comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// `token`: the token itself, which may name environment variables.
    Value(String),
    /// `token_file`: a file holding just the token, such as one in a folder
    /// synced between the host and its clients.
    File(String),
    /// Neither: hey keeps the token itself.
    Kept,
}

impl TokenSource {
    fn new(token: Option<String>, token_file: Option<String>) -> Result<Self, &'static str> {
        match (token, token_file) {
            (Some(_), Some(_)) => Err("set either `token` or `token_file`, not both"),
            (Some(token), None) => Ok(Self::Value(token)),
            (None, Some(path)) => Ok(Self::File(path)),
            (None, None) => Ok(Self::Kept),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("hey"))
}

pub fn data_dir() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|dir| dir.join("hey"))
        .context("no data directory on this platform")
}

pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("hey"))
}

/// Expands `~` and environment variables, as a shell would.
pub fn expand_path(path: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(expand(path.trim())?))
}

/// Expands `~` and `$VAR`/`${VAR}`, so secrets can stay in the environment.
pub fn expand(text: &str) -> Result<String> {
    shellexpand::full(text)
        .map(|expanded| expanded.into_owned())
        .with_context(|| format!("cannot expand {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_extend_defaults_and_servers_are_local_or_remote() {
        let config: Config = toml::from_str(
            r#"
            model = "anthropic/claude-opus-5-5"
            thinking = "high"

            [tools.permissions]
            shell = "allow"
            "mcp:github" = "deny"

            [mcp_servers.github]
            command = "github-mcp-server"
            args = ["stdio"]

            [mcp_servers.docs]
            type = "remote"
            url = "https://example.com/mcp"
            headers = { Authorization = "Bearer ${TOKEN}" }
            "#,
        )
        .unwrap();

        assert_eq!(config.tools.default, Permission::Confirm);
        assert_eq!(config.thinking, Some(Thinking::High));
        assert_eq!(config.tools.permissions["read"], Permission::Allow);
        assert_eq!(config.tools.permissions["shell"], Permission::Allow);
        assert_eq!(config.tools.permissions["mcp:github"], Permission::Deny);
        assert!(matches!(config.mcp_servers["github"], McpServer::Stdio { .. }));
        assert!(matches!(config.mcp_servers["docs"], McpServer::Http { .. }));
        assert!(config.display.syntax_highlighting);
    }

    #[test]
    fn documented_defaults_match_the_code() {
        let documented: Config = toml::from_str(include_str!("../defaults.toml")).unwrap();
        let built_in = Config::default();
        assert_eq!(documented.model, built_in.model);
        assert_eq!(documented.max_turns, built_in.max_turns);
        assert_eq!(documented.display.theme, built_in.display.theme);
        assert_eq!(documented.display.wrap_width, built_in.display.wrap_width);
        assert_eq!(documented.editor.mode, built_in.editor.mode);
        assert_eq!(documented.editor.history_size, built_in.editor.history_size);
        assert_eq!(documented.memory.enabled, built_in.memory.enabled);
        assert_eq!(documented.memory.recall, built_in.memory.recall);
        assert_eq!(documented.memory.embedding_model, built_in.memory.embedding_model);
        assert_eq!(documented.tools.allow_all, built_in.tools.allow_all);
        assert_eq!(documented.tools.default, built_in.tools.default);
        assert_eq!(documented.tools.permissions, built_in.tools.permissions);
        assert_eq!(documented.server.listen, built_in.server.listen);
    }

    #[test]
    fn rejects_mistakes_with_a_reason() {
        let error = |text| toml::from_str::<Config>(text).unwrap_err().to_string();
        assert!(error("[mcp_servers.x]\nurl = \"u\"\nargs = [\"a\"]").contains("only apply to a local"));
        assert!(error("[mcp_servers.x]\nargs = []").contains("set `command`"));
        assert!(error("[mcp_servers.x]\ntype = \"http\"\ncommand = \"c\"").contains("not a `command`"));
        assert!(error("wrap_width = 80").contains("unknown field"));
        assert!(error("[tools]\ndefault = \"maybe\"").contains("unknown variant"));
        assert!(error("thinking = \"huge\"").contains("unknown variant"));
        assert!(error("[server]\ntoken = \"t\"\ntoken_file = \"f\"").contains("not both"));
    }

    #[test]
    fn tokens_are_given_read_from_a_file_or_kept_by_hey() {
        let config: Config = toml::from_str(
            r#"
            [server]
            token_file = "~/Vault/hey-token"

            [remotes.home]
            url = "http://home:7700"
            token = "${HEY_TOKEN}"

            [remotes.work]
            url = "http://work:7700"
            "#,
        )
        .unwrap();
        assert_eq!(config.server.token, TokenSource::File("~/Vault/hey-token".into()));
        assert_eq!(config.remotes["home"].token, TokenSource::Value("${HEY_TOKEN}".into()));
        assert_eq!(config.remotes["work"].token, TokenSource::Kept);
        assert_eq!(Config::default().server.token, TokenSource::Kept);
    }
}
