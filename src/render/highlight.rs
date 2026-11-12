/* -------------------------------------------------------------------------- */
/*                             render/highlight.rs                            */
/* -------------------------------------------------------------------------- */

use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use bat::assets::HighlightingAssets;
use syntect::highlighting::{
    Color, HighlightIterator, HighlightState, Highlighter as Painter, Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use yansi::Paint;

static SYNTAXES: LazyLock<Option<SyntaxSet>> =
    LazyLock::new(|| HighlightingAssets::from_binary().get_syntax_set().ok().cloned());

pub fn themes() -> Vec<String> {
    HighlightingAssets::from_binary()
        .themes()
        .map(str::to_owned)
        .collect()
}

/// Highlights Markdown line by line, carrying state across lines so fenced
/// code blocks keep their language.
pub struct Highlighter {
    theme: Theme,
    parse: ParseState,
    highlight: HighlightState,
}

impl Highlighter {
    pub fn new(theme: &str) -> Result<Self> {
        let syntaxes = SYNTAXES.as_ref().context("the bundled syntax set failed to load")?;
        let markdown = syntaxes
            .find_syntax_by_name("Markdown")
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        let assets = HighlightingAssets::from_binary();
        if !assets.themes().any(|name| name == theme) {
            bail!("unknown theme `{theme}`");
        }
        let theme = assets.get_theme(theme).clone();
        let highlight = HighlightState::new(&Painter::new(&theme), ScopeStack::new());
        Ok(Self {
            parse: ParseState::new(markdown),
            highlight,
            theme,
        })
    }

    pub fn line(&mut self, line: &str) -> String {
        let ops = SYNTAXES
            .as_ref()
            .and_then(|syntaxes| self.parse.parse_line(line, syntaxes).ok());
        let Some(ops) = ops else {
            return line.to_owned();
        };
        let painter = Painter::new(&self.theme);
        HighlightIterator::new(&mut self.highlight, &ops, line, &painter)
            .map(|(style, text)| paint(text, style.foreground))
            .collect()
    }
}

/// bat's ANSI themes keep a palette index in the red channel with alpha 0,
/// and the terminal's default colour as alpha 1.
fn paint(text: &str, color: Color) -> String {
    match color.a {
        0 => text.fixed(color.r).to_string(),
        1 => text.to_string(),
        _ => text.rgb(color.r, color.g, color.b).to_string(),
    }
}
