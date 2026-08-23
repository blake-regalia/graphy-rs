//! `graphy-lsp` binary: a language server for the RDF text formats and SPARQL
//! over stdio (docs/10). All logic lives in the library crate; this is just the
//! entry point.

fn main() {
    if let Err(e) = graphy_lsp::server::run_stdio() {
        eprintln!("graphy-lsp: fatal: {e}");
        std::process::exit(1);
    }
}
