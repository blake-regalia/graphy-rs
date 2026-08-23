// Minimal LSP client: launch graphy-lsp over stdio for the three languages.
const { LanguageClient } = require('vscode-languageclient/node');

let client;

function activate(_context) {
  const command = process.env.GRAPHY_LSP_BIN;
  if (!command) throw new Error('GRAPHY_LSP_BIN not set');
  client = new LanguageClient(
    'graphy-lsp-smoke',
    'graphy-lsp',
    { command },
    {
      documentSelector: [
        { language: 'turtle' },
        { language: 'sparql' },
        { language: 'jsonld' },
      ],
    }
  );
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
