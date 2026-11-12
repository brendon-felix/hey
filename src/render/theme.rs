/* -------------------------------------------------------------------------- */
/*                               render/theme.rs                              */
/* -------------------------------------------------------------------------- */

use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use anyhow::{Result, bail};
use bat::assets::HighlightingAssets;
use syntect::highlighting::{Color as Rgba, Highlighter as Painter, Theme as Syntax};
use syntect::parsing::Scope;
use yansi::{Paint, Painted};

/// What hey draws everything in: replies are highlighted with the syntax
/// theme, and hey's own text takes its colours from the palette.
pub struct Theme {
    pub name: String,
    pub syntax: Syntax,
    pub palette: Palette,
}

/// Until a theme is configured: no highlighting, and the terminal's colours.
static CURRENT: LazyLock<RwLock<Arc<Theme>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Theme::new(String::new(), Syntax::default()))));

impl Theme {
    pub fn load(name: &str) -> Result<Self> {
        let assets = HighlightingAssets::from_binary();
        if !assets.themes().any(|theme| theme == name) {
            bail!("unknown theme `{name}`");
        }
        Ok(Self::new(name.to_owned(), assets.get_theme(name).clone()))
    }

    fn new(name: String, syntax: Syntax) -> Self {
        let palette = Palette::of(&syntax);
        Self {
            name,
            syntax,
            palette,
        }
    }
}

pub fn themes() -> Vec<String> {
    HighlightingAssets::from_binary()
        .themes()
        .map(str::to_owned)
        .collect()
}

pub fn current() -> Arc<Theme> {
    CURRENT.read().unwrap_or_else(PoisonError::into_inner).clone()
}

pub fn set(theme: Theme) {
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(theme);
}

pub fn palette() -> Palette {
    current().palette
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The terminal's own foreground.
    Default,
    /// An index into the terminal's palette.
    Ansi(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn paint<T: ?Sized>(self, value: &T) -> Painted<&T> {
        value.fg(self.into())
    }
}

impl From<Rgba> for Color {
    /// bat's ANSI themes keep a palette index in the red channel with alpha
    /// 0, and the terminal's default colour as alpha 1.
    fn from(color: Rgba) -> Self {
        match color.a {
            0 => Self::Ansi(color.r),
            1 => Self::Default,
            _ => Self::Rgb(color.r, color.g, color.b),
        }
    }
}

impl From<Color> for yansi::Color {
    fn from(color: Color) -> Self {
        match color {
            Color::Default => Self::Primary,
            Color::Ansi(index) => Self::Fixed(index),
            Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

/// The theme's colours for hey's own text, by role. Status signals (success,
/// warnings, errors) keep the terminal's green, yellow and red.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// What the user writes.
    pub input: Color,
    /// The prompt, and marks that draw the eye.
    pub accent: Color,
    /// Things hey recognises: commands, mentions, tools.
    pub name: Color,
    /// Arguments to a command.
    pub value: Color,
    /// Detail that should recede: hints, status, notices.
    pub muted: Color,
}

impl Palette {
    /// The colours of scopes every theme styles, falling back along related
    /// scopes and then to the terminal's palette, since some themes leave even
    /// `string` or `keyword` unstyled.
    fn of(syntax: &Syntax) -> Self {
        let painter = Painter::new(syntax);
        let scope = |names: &[&str]| {
            names
                .iter()
                .filter_map(|name| Scope::new(name).ok())
                .find_map(|scope| painter.style_mod_for_stack(&[scope]).foreground)
                .map(Color::from)
        };
        Self {
            input: scope(&["string", "markup.inserted"]).unwrap_or(Color::Ansi(2)),
            accent: scope(&["keyword", "storage.type"]).unwrap_or(Color::Ansi(5)),
            name: scope(&["entity.name.function", "support.function", "markup.heading"])
                .unwrap_or(Color::Ansi(6)),
            value: scope(&["variable.parameter", "constant"]).unwrap_or(Color::Ansi(4)),
            // Palette themes leave colours to the terminal, whose bright black
            // is its muted one; their comments may share the input's colour.
            muted: match scope(&["comment"]) {
                Some(rgb @ Color::Rgb(..)) => rgb,
                _ => Color::Ansi(8),
            },
        }
    }
}
