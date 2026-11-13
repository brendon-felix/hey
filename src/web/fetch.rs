/* -------------------------------------------------------------------------- */
/*                                web/fetch.rs                                */
/* -------------------------------------------------------------------------- */

use dom_smoothie::{Config, Readability, TextMode};
use encoding_rs::{Encoding, UTF_8};
use mime::Mime;
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext, ToolExecutionError, args_schema};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{network_error, status_error};

/// The largest page downloaded, so a link to an archive or a video is not
/// taken in whole.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Markdown first: some sites, and every one behind Cloudflare's "Markdown
/// for Agents", send it in place of HTML when asked.
const ACCEPT_TYPES: &str =
    "text/markdown, text/html;q=0.9, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.5";

/// Elements with nothing to read, which the whole-page conversion drops.
const SKIPPED_TAGS: [&str; 6] = ["script", "style", "noscript", "template", "svg", "iframe"];

const GUIDANCE: &str = "Fetch a web page and give its main content as Markdown, or a text file \
(plain text, Markdown, JSON, XML) as it is. It does not run JavaScript, so a page built by its \
scripts comes back empty or nearly so. Set `whole` for pages whose content is mostly links, such \
as indexes and listings. Use it for any URL, not `curl` in `shell`.";

/// Downloads a page and converts it to Markdown here, which takes a few
/// milliseconds, rather than having a browser render it.
pub struct Fetch {
    pub http: reqwest::Client,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {
    /// The page's http or https URL.
    pub url: String,
    /// Convert the whole page, navigation and all, instead of only its main content.
    #[serde(default)]
    pub whole: bool,
}

impl Tool for Fetch {
    const NAME: &'static str = "fetch";
    type Args = FetchArgs;
    type Output = String;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        GUIDANCE.into()
    }

    fn parameters(&self) -> serde_json::Value {
        args_schema::<FetchArgs>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_read_only(true)
    }

    async fn call(&self, _context: &mut ToolContext, args: FetchArgs) -> Result<String, ToolExecutionError> {
        let url = Url::parse(&args.url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| {
                ToolExecutionError::invalid_args(format!("`{}` is not an http or https URL", args.url))
            })?;
        let page = self.download(url).await?;
        tokio::task::spawn_blocking(move || page.into_markdown(args.whole))
            .await
            .map_err(|error| ToolExecutionError::other(error.to_string()))?
    }
}

impl Fetch {
    async fn download(&self, url: Url) -> Result<Page, ToolExecutionError> {
        let what = url.to_string();
        let mut response = self
            .http
            .get(url)
            .header(ACCEPT, ACCEPT_TYPES)
            .send()
            .await
            .map_err(|error| network_error(&what, error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(&what, status, ""));
        }
        let content_type: Mime = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(mime::TEXT_HTML);
        let kind = Kind::of(&content_type).ok_or_else(|| {
            ToolExecutionError::invalid_args(format!(
                "{what} is {}, not a web page or text",
                content_type.essence_str()
            ))
        })?;
        let url = response.url().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| network_error(&what, error))? {
            if body.len() + chunk.len() > MAX_PAGE_BYTES {
                return Err(ToolExecutionError::other(format!(
                    "{what} is larger than {} MiB",
                    MAX_PAGE_BYTES / (1024 * 1024)
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let encoding = content_type
            .get_param(mime::CHARSET)
            .and_then(|charset| Encoding::for_label(charset.as_str().as_bytes()))
            .unwrap_or(UTF_8);
        Ok(Page {
            url,
            kind,
            text: encoding.decode(&body).0.into_owned(),
        })
    }
}

/// A page as downloaded, from `url` after any redirects.
struct Page {
    url: Url,
    kind: Kind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    /// Text the model reads as it is.
    Text,
}

impl Kind {
    fn of(content_type: &Mime) -> Option<Self> {
        let suffix = content_type.suffix().map(|suffix| suffix.as_str());
        match (content_type.type_().as_str(), content_type.subtype().as_str(), suffix) {
            ("text", "html", _) | ("application", "xhtml", _) => Some(Kind::Html),
            ("text", ..)
            | ("application", "json" | "xml" | "javascript" | "yaml" | "toml", _)
            | ("application", _, Some("json" | "xml")) => Some(Kind::Text),
            _ => None,
        }
    }
}

impl Page {
    /// The page as the model reads it: its URL, then its main content,
    /// unless it has none Readability can find or `whole` asks for all of it.
    fn into_markdown(self, whole: bool) -> Result<String, ToolExecutionError> {
        let content = match (self.kind, whole) {
            (Kind::Text, _) => self.text,
            (Kind::Html, false) => match main_content(&self.text, &self.url) {
                Some(article) => article,
                None => whole_page(&self.text)?,
            },
            (Kind::Html, true) => whole_page(&self.text)?,
        };
        Ok(format!("URL: {}\n\n{content}", self.url))
    }
}

/// The article in `html`, as Readability (Firefox's Reader View) finds it.
fn main_content(html: &str, url: &Url) -> Option<String> {
    let config = Config {
        text_mode: TextMode::Markdown,
        ..Config::default()
    };
    let article = Readability::new(html, Some(url.as_str()), Some(config))
        .and_then(|mut readability| readability.parse())
        .ok()?;
    let text = article.text_content.trim();
    match (text.is_empty(), article.title.trim()) {
        (true, _) => None,
        (false, "") => Some(text.to_owned()),
        (false, title) => Some(format!("# {title}\n\n{text}")),
    }
}

fn whole_page(html: &str) -> Result<String, ToolExecutionError> {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .build()
        .convert(html)
        .map_err(|error| ToolExecutionError::other(format!("cannot convert the page: {error}")))
}
