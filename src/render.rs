/* -------------------------------------------------------------------------- */
/*                                 render.rs                                  */
/* -------------------------------------------------------------------------- */

use std::io::{Write, stdout};
use std::rc::Rc;
use std::thread::sleep;
use std::time::Duration;

use bat::assets::HighlightingAssets;
use syntect::{dumps::from_uncompressed_data, easy::HighlightLines, parsing::SyntaxSet};
use yansi::Paint;

/* -------------------------------------------------------------------------- */

pub struct Highlighter {
    _highlighting_assets: Rc<HighlightingAssets>,
    highlighter: HighlightLines<'static>,
    syntax_set: SyntaxSet,
}

const SYNTAX_SET: &[u8] = include_bytes!("../syntax_set.bin");
const MAX_LINE_LENGTH: usize = 100;

impl Highlighter {
    pub fn new() -> Self {
        let ss: SyntaxSet = from_uncompressed_data(SYNTAX_SET).expect("Failed to load syntax set");
        let syntax_ref = ss
            .find_syntax_by_name("Markdown")
            .expect("Failed to find syntax");

        let highlighting_assets = Rc::new(HighlightingAssets::from_binary());
        let theme = highlighting_assets.get_theme("ansi");

        // Safety: We know that highlighting_assets will live as long as the struct
        // since it's an Rc field in the same struct
        let theme_static: &'static _ = unsafe { std::mem::transmute(theme) };
        let highlighter = HighlightLines::new(&syntax_ref, theme_static);

        Highlighter {
            _highlighting_assets: highlighting_assets,
            highlighter,
            syntax_set: ss,
        }
    }

    pub fn highlight_line(&mut self, line: &str) -> String {
        let ranges = self
            .highlighter
            .highlight_line(line, &self.syntax_set)
            .expect("Failed to highlight line");
        ranges
            .iter()
            .map(|(style, text)| {
                // for ansi theme, red channel is an index
                match style.foreground.r {
                    // use yansi::Paint to colorize
                    0 => text.primary(),
                    1 => text.red(),
                    2 => text.green(),   // strings and stuff
                    3 => text.yellow(),  // literals and numbers
                    4 => text.blue(),    // headers and function names
                    5 => text.magenta(), // keywords
                    6 => text.white(),
                    7 => text.black(),
                    _ => text.primary(),
                }
                .to_string()
            })
            .collect::<String>()
    }
}

fn wrap_line(line: &str) -> String {
    let term_width = term_size::dimensions()
        .map(|(w, _)| w)
        .expect("Failed to get terminal width");
    let max_width = term_width.min(MAX_LINE_LENGTH);
    textwrap::wrap(line, max_width).join("\n")
}

pub fn animate_line(line: &str, num_micros: u64) {
    line.chars().for_each(|c| {
        print!("{}", c);
        stdout().lock().flush().unwrap();
        sleep(Duration::from_micros(num_micros));
    });
}

pub fn render_line(line: &str, highlighter: &mut Highlighter) {
    let line = highlighter.highlight_line(line);
    let line = wrap_line(&line);
    animate_line(&line, 500);
}
