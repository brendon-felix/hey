/* -------------------------------------------------------------------------- */
/*                                 render.rs                                  */
/* -------------------------------------------------------------------------- */

use ansi_parser::{AnsiParser, Output};
use anyhow::{Context, Result};
use std::io::{Write, stdout};
use std::rc::Rc;
use std::thread::sleep;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

use bat::assets::HighlightingAssets;
use syntect::{dumps::from_uncompressed_data, easy::HighlightLines, parsing::SyntaxSet};
use yansi::Paint;

/* -------------------------------------------------------------------------- */

pub struct Highlighter {
    _highlighting_assets: Rc<HighlightingAssets>,
    highlighter: HighlightLines<'static>,
    syntax_set: SyntaxSet,
    theme_name: String,
}

const SYNTAX_SET: &[u8] = include_bytes!("../syntax_set.bin");
const MAX_LINE_LENGTH: usize = 100;

impl Highlighter {
    pub fn new(theme_name: &str) -> Result<Self> {
        let ss: SyntaxSet =
            from_uncompressed_data(SYNTAX_SET).context("Failed to load syntax set")?;

        let syntax_ref = ss
            .find_syntax_by_name("Markdown")
            .unwrap_or_else(|| ss.find_syntax_plain_text());

        let highlighting_assets = Rc::new(HighlightingAssets::from_binary());
        let theme = highlighting_assets.get_theme(theme_name);

        // highlighting_assets will live as long as the struct
        let theme_static: &'static _ = unsafe { std::mem::transmute(theme) };
        let highlighter = HighlightLines::new(&syntax_ref, theme_static);

        Ok(Highlighter {
            _highlighting_assets: highlighting_assets,
            highlighter,
            syntax_set: ss,
            theme_name: theme_name.to_string(),
        })
    }

    pub fn highlight_line(&mut self, line: &str) -> String {
        let theme_is_ansi = &self.theme_name == "ansi"
            || &self.theme_name == "base16"
            || &self.theme_name == "base16-256";

        let ranges = match self.highlighter.highlight_line(line, &self.syntax_set) {
            Ok(ranges) => ranges,
            Err(_) => {
                return line.to_string();
            }
        };

        ranges
            .iter()
            .map(|(style, text)| {
                // for ansi theme, red channel is an index
                if theme_is_ansi {
                    match style.foreground.r {
                        0 => text.primary(),
                        1 => text.red(),
                        2 => text.green(),
                        3 => text.yellow(),
                        4 => text.blue(),
                        5 => text.magenta(),
                        6 => text.white(),
                        7 => text.black(),
                        _ => text.primary(),
                    }
                    .to_string()
                } else {
                    let fg = style.foreground;
                    text.rgb(fg.r, fg.g, fg.b).to_string()
                }
            })
            .collect::<String>()
    }
}

pub fn wrap_line(line: &str) -> String {
    let term_width = term_size::dimensions().map(|(w, _)| w).unwrap_or(80);
    let max_width = term_width.min(MAX_LINE_LENGTH);
    textwrap::wrap(line, max_width).join("\n")
}

pub fn render_line(line: &str, highlighter: &mut Highlighter) -> Result<()> {
    let line = highlighter.highlight_line(line);
    let line = wrap_line(&line);
    snailprint(&line, 5000);
    Ok(())
}

pub fn snailprint(text: &str, num_micros: u64) {
    text.ansi_parse().for_each(|output| match output {
        Output::TextBlock(t) => {
            t.graphemes(true).for_each(|g| {
                sleep(Duration::from_micros(num_micros));
                print!("{}", g);
                let _ = stdout().flush();
            });
        }
        Output::Escape(esc) => {
            print!("{}", esc);
            let _ = stdout().flush();
        }
    });
    let _ = stdout().flush();
}
