// LSP client for graphy-lsp: launches the server binary over stdio for the
// Turtle family, SPARQL, and JSON-LD.
const fs = require('fs');
const path = require('path');
const vscode = require('vscode');
const { LanguageClient } = require('vscode-languageclient/node');

let client;

/** Resolve the server binary: setting > env var > bundled > PATH. */
function serverCommand(context) {
  const configured = vscode.workspace
    .getConfiguration('graphy-lsp')
    .get('serverPath');
  if (configured && fs.existsSync(configured)) return configured;

  const env = process.env.GRAPHY_LSP_BIN;
  if (env && fs.existsSync(env)) return env;

  const exe = process.platform === 'win32' ? 'graphy-lsp.exe' : 'graphy-lsp';
  const bundled = context.asAbsolutePath(path.join('server', exe));
  if (fs.existsSync(bundled)) return bundled;

  // Fall back to PATH lookup; the client will surface a spawn error if absent.
  return exe;
}

function activate(context) {
  const command = serverCommand(context);
  client = new LanguageClient(
    'graphy-lsp',
    'Graphy LSP',
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
