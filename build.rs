use bat::assets::HighlightingAssets;
use std::path::PathBuf;

fn main() {
    let syntax_set = HighlightingAssets::from_binary()
        .get_syntax_set()
        .unwrap()
        .clone();
    let syntax_set_builder = syntax_set.into_builder();
    let syntax_set = syntax_set_builder.build();
    let out_path = PathBuf::from("syntax_set.bin");
    syntect::dumps::dump_to_uncompressed_file(&syntax_set, out_path).unwrap();
}
