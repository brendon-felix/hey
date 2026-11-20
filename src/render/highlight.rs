/* -------------------------------------------------------------------------- */
/*                             render/highlight.rs                            */
/* -------------------------------------------------------------------------- */

use std::ops::Range;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result};
use bat::assets::HighlightingAssets;
use syntect::highlighting::{
    HighlightState, Highlighter as Painter, RangedHighlightIterator, Theme as Syntax,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use super::theme::{Color, Theme};

static SYNTAXES: LazyLock<Option<SyntaxSet>> = LazyLock::new(|| {
    HighlightingAssets::from_binary()
        .get_syntax_set()
        .ok()
        .cloned()
});

/// A run of a line in one colour, by byte range.
pub type Span = (Color, Range<usize>);

/// Highlights Markdown line by line, carrying state across lines so fenced
/// code blocks keep their language.
pub struct Highlighter {
    theme: Arc<Theme>,
    syntaxes: &'static SyntaxSet,
    parse: ParseState,
    highlight: HighlightState,
}

impl Highlighter {
    pub fn new(theme: Arc<Theme>) -> Result<Self> {
        let syntaxes = SYNTAXES
            .as_ref()
            .context("the bundled syntax set failed to load")?;
        let markdown = syntaxes
            .find_syntax_by_name("Markdown")
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        Ok(Self::with(theme, syntaxes, markdown))
    }

    /// A highlighter for the language of the file at `path`, when it has one.
    pub fn for_path(theme: Arc<Theme>, path: &str) -> Option<Self> {
        let syntaxes = SYNTAXES.as_ref()?;
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let syntax = syntaxes.find_syntax_by_extension(name).or_else(|| {
            name.rsplit_once('.')
                .and_then(|(_, ext)| syntaxes.find_syntax_by_extension(ext))
        })?;
        Some(Self::with(theme, syntaxes, syntax))
    }

    fn with(
        theme: Arc<Theme>,
        syntaxes: &'static SyntaxSet,
        syntax: &syntect::parsing::SyntaxReference,
    ) -> Self {
        let highlight = HighlightState::new(&Painter::new(&theme.syntax), ScopeStack::new());
        Self {
            parse: ParseState::new(syntax),
            highlight,
            syntaxes,
            theme,
        }
    }

    /// Highlights the next complete line, `\n` included.
    pub fn line(&mut self, line: &str) -> Vec<Span> {
        spans(
            &self.theme.syntax,
            self.syntaxes,
            &mut self.parse,
            &mut self.highlight,
            line,
        )
    }

    /// Highlights the beginning of the next line as if it were all of it,
    /// leaving the state for when the line is complete.
    pub fn preview(&self, line: &str) -> Vec<Span> {
        let (mut parse, mut highlight) = (self.parse.clone(), self.highlight.clone());
        spans(
            &self.theme.syntax,
            self.syntaxes,
            &mut parse,
            &mut highlight,
            line,
        )
    }
}

fn spans(
    syntax: &Syntax,
    syntaxes: &SyntaxSet,
    parse: &mut ParseState,
    highlight: &mut HighlightState,
    line: &str,
) -> Vec<Span> {
    let Ok(ops) = parse.parse_line(line, syntaxes) else {
        return vec![(Color::Default, 0..line.len())];
    };
    let painter = Painter::new(syntax);
    RangedHighlightIterator::new(highlight, &ops, line, &painter)
        .map(|(style, _, range)| (Color::from(style.foreground), range))
        .collect()
}
