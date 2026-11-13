/* -------------------------------------------------------------------------- */
/*                               web/search.rs                                */
/* -------------------------------------------------------------------------- */

use std::collections::HashMap;
use std::fmt::Write;

use reqwest::header::ACCEPT;
use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{network_error, status_error};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/llm/context";

const GUIDANCE: &str = "Search the web and give the most relevant passages of the pages found, \
each under its page's title, URL and date. Use it for anything current or outside this machine, \
then `fetch` a page when its passages are not enough.";

/// Searches with Brave's LLM Context API, which answers with passages it has
/// already extracted from the pages found, so one call takes the place of a
/// search and a fetch of each result.
pub struct WebSearch {
    pub http: reqwest::Client,
    pub key: String,
    pub result_bytes: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArgs {
    /// What to search for, as you would type it into a search engine. Operators such as
    /// `site:` work.
    pub query: String,
    /// Only pages from within the last day, week, month or year.
    #[serde(default)]
    pub freshness: Option<Freshness>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    fn code(self) -> &'static str {
        match self {
            Freshness::Day => "pd",
            Freshness::Week => "pw",
            Freshness::Month => "pm",
            Freshness::Year => "py",
        }
    }
}

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Args = WebSearchArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<WebSearchArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_read_only(true)
    }

    async fn call(&self, _context: &mut ToolContext, args: WebSearchArgs) -> Result<String, ToolExecutionError> {
        let tokens = self.token_budget().to_string();
        let mut query = vec![("q", args.query.as_str()), ("maximum_number_of_tokens", &tokens)];
        query.extend(args.freshness.map(|freshness| ("freshness", freshness.code())));
        let what = "Brave Search";
        let response = self
            .http
            .get(ENDPOINT)
            .header("X-Subscription-Token", &self.key)
            .header(ACCEPT, "application/json")
            .query(&query)
            .send()
            .await
            .map_err(|error| network_error(what, error))?;
        let status = response.status();
        let body = response.text().await.map_err(|error| network_error(what, error))?;
        if !status.is_success() {
            return Err(status_error(what, status, &body));
        }
        let context: Context = serde_json::from_str(&body)
            .map_err(|error| ToolExecutionError::provider(format!("{what} answered unexpectedly: {error}")))?;
        Ok(context.render(&args.query))
    }
}

impl WebSearch {
    /// Tokens of passages to ask for: what the model reads of a result, at
    /// about four bytes a token, within the range the API takes.
    fn token_budget(&self) -> usize {
        (self.result_bytes / 4).clamp(1024, 32768)
    }
}

#[derive(Debug, Deserialize)]
struct Context {
    grounding: Grounding,
    #[serde(default)]
    sources: HashMap<String, Source>,
}

/// Only the general results: places come only with location-aware search,
/// which is not asked for.
#[derive(Debug, Deserialize)]
struct Grounding {
    #[serde(default)]
    generic: Vec<Passages>,
}

#[derive(Debug, Deserialize)]
struct Passages {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    snippets: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Source {
    /// The page's date as a full date, `YYYY-MM-DD`, an age and an ISO 8601
    /// timestamp, or nothing when it has none.
    #[serde(default)]
    age: Vec<String>,
}

impl Context {
    fn render(&self, query: &str) -> String {
        if self.grounding.generic.is_empty() {
            return format!("No results for `{query}`.");
        }
        let mut text = String::new();
        for page in &self.grounding.generic {
            let date = self
                .sources
                .get(&page.url)
                .and_then(|source| source.age.get(1))
                .map(|date| format!(" ({date})"))
                .unwrap_or_default();
            let _ = writeln!(text, "## {}\n{}{date}\n", page.title.trim(), page.url);
            for snippet in &page.snippets {
                let _ = writeln!(text, "{}\n", snippet.trim());
            }
        }
        text.truncate(text.trim_end().len());
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_passages_under_their_page() {
        let context: Context = serde_json::from_str(
            r#"{
                "grounding": {
                    "generic": [
                        {"url": "https://a.example", "title": "A", "snippets": ["one", "two"]},
                        {"url": "https://b.example", "title": "B", "snippets": ["three"]}
                    ],
                    "map": []
                },
                "sources": {
                    "https://a.example": {"title": "A", "hostname": "a.example",
                        "age": ["Monday, January 15, 2024", "2024-01-15", "380 days ago", "2024-01-15T13:45:02Z"]},
                    "https://b.example": {"title": "B", "hostname": "b.example", "age": []}
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            context.render("q"),
            "## A\nhttps://a.example (2024-01-15)\n\none\n\ntwo\n\n## B\nhttps://b.example\n\nthree"
        );
        let empty: Context = serde_json::from_str(r#"{"grounding": {"generic": []}}"#).unwrap();
        assert_eq!(empty.render("q"), "No results for `q`.");
    }
}
