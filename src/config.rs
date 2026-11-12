/* -------------------------------------------------------------------------- */
/*                                 config.rs                                  */
/* -------------------------------------------------------------------------- */

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};

use crate::permissions::{self, Permission};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u64>,
    pub max_turns: usize,
    pub greetings: bool,
    pub providers: Providers,
    pub display: Display,
    pub editor: Editor,
    pub sessions: Sessions,
    pub tools: Tools,
    pub mcp_servers: BTreeMap<String, McpServer>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "openai/gpt-5.5".into(),
            system_prompt: None,
            max_tokens: None,
            max_turns: 50,
            greetings: true,
            providers: Providers::default(),
            display: Display::default(),
            editor: Editor::default(),
            sessions: Sessions::default(),
            tools: Tools::default(),
            mcp_servers: BTreeMap::new(),
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
            None => dirs::data_dir()
                .map(|dir| dir.join("hey").join("sessions"))
                .context("no data directory on this platform; set `sessions.dir`"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tools {
    pub default: Permission,
    #[serde(deserialize_with = "over_default_rules")]
    pub permissions: BTreeMap<String, Permission>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
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

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("hey"))
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
        assert_eq!(documented.tools.default, built_in.tools.default);
        assert_eq!(documented.tools.permissions, built_in.tools.permissions);
    }

    #[test]
    fn rejects_mistakes_with_a_reason() {
        let error = |text| toml::from_str::<Config>(text).unwrap_err().to_string();
        assert!(error("[mcp_servers.x]\nurl = \"u\"\nargs = [\"a\"]").contains("only apply to a local"));
        assert!(error("[mcp_servers.x]\nargs = []").contains("set `command`"));
        assert!(error("[mcp_servers.x]\ntype = \"http\"\ncommand = \"c\"").contains("not a `command`"));
        assert!(error("wrap_width = 80").contains("unknown field"));
        assert!(error("[tools]\ndefault = \"maybe\"").contains("unknown variant"));
    }
}
