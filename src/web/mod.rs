/* -------------------------------------------------------------------------- */
/*                                 web/mod.rs                                 */
/* -------------------------------------------------------------------------- */

mod fetch;
mod search;

use std::time::Duration;

use anyhow::Result;
use rig_core::tool::{Tool, ToolExecutionError};

pub use fetch::Fetch;
pub use search::WebSearch;

/// Every web tool, offered or not.
pub const TOOLS: [&str; 2] = [Fetch::NAME, WebSearch::NAME];

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; hey/",
    env!("CARGO_PKG_VERSION"),
    "; +",
    env!("CARGO_PKG_HOMEPAGE"),
    ")"
);

/// The tools that read the web: `fetch` always, and `web_search` when
/// `BRAVE_API_KEY` is set, as a search needs an index to search.
#[derive(Clone)]
pub struct Web {
    http: reqwest::Client,
    brave_key: Option<String>,
    /// Bytes of search results to ask for, to fit what the model reads.
    result_bytes: usize,
}

impl Web {
    pub fn new(result_bytes: usize) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        let brave_key = std::env::var("BRAVE_API_KEY")
            .ok()
            .filter(|key| !key.is_empty());
        Ok(Self {
            http,
            brave_key,
            result_bytes,
        })
    }

    pub fn fetch(&self) -> Fetch {
        Fetch {
            http: self.http.clone(),
        }
    }

    pub fn search(&self) -> Option<WebSearch> {
        self.brave_key.as_ref().map(|key| WebSearch {
            http: self.http.clone(),
            key: key.clone(),
            result_bytes: self.result_bytes,
        })
    }

    /// The names of the tools offered.
    pub fn tools(&self) -> impl Iterator<Item = &'static str> {
        let search = self.brave_key.as_ref().map(|_| WebSearch::NAME);
        std::iter::once(Fetch::NAME).chain(search)
    }
}

/// The error for an unsuccessful `status` from `what`, with the start of
/// its `body`, which often says why.
fn status_error(what: &str, status: reqwest::StatusCode, body: &str) -> ToolExecutionError {
    use reqwest::StatusCode;
    let body = body.trim();
    let reason = match body.is_empty() {
        true => String::new(),
        false => format!(": {}", &body[..body.floor_char_boundary(500)]),
    };
    let message = format!("{what} answered {status}{reason}");
    match status {
        StatusCode::NOT_FOUND | StatusCode::GONE => ToolExecutionError::not_found(message),
        StatusCode::TOO_MANY_REQUESTS => ToolExecutionError::rate_limited(message),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ToolExecutionError::permission_denied(message)
        }
        _ => ToolExecutionError::provider(message),
    }
}

fn network_error(what: &str, error: reqwest::Error) -> ToolExecutionError {
    match error.is_timeout() {
        true => ToolExecutionError::timeout(format!("{what} did not answer in time")),
        false => {
            let error = anyhow::Error::from(error);
            ToolExecutionError::network(format!("cannot reach {what}: {error:#}"))
        }
    }
}
