# Graphy LSP for VS Code

This extension connects VS Code to `graphy-lsp` for Turtle, TriG, N-Triples, N-Quads, SPARQL, and JSON-LD files.

It provides semantic highlighting, diagnostics and quick fixes, completion, hover, document symbols, folding ranges, and formatting. JSON-LD assistance is lexical; remote contexts are not resolved.

## Package and install

```sh
cargo build --release -p graphy-lsp
cd editors/vscode
npm ci
mkdir -p server
npm run package
code --install-extension graphy-lsp-0.1.0.vsix
```

`npm run package` copies the release server binary into the extension before creating the VSIX.

## Server selection

The extension tries these locations in order:

1. the `graphy-lsp.serverPath` setting;
2. the `GRAPHY_LSP_BIN` environment variable;
3. the bundled `server/graphy-lsp` binary;
4. `graphy-lsp` on `PATH`.

## Development

Open this directory in VS Code and press `F5`. The default build task compiles the release server and links it into `server/` for the Extension Development Host. Fixtures are under `../vscode-smoke/fixtures`.

After rebuilding, restart the development host with `Ctrl+Shift+F5`. Semantic highlighting requires a theme with semantic-token support; the extension also includes Graphy themes.
