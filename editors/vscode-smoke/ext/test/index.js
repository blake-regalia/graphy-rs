// Runs inside the VS Code extension host (--extensionTestsPath).
// Opens one fixture per language and asserts semantic tokens, document
// symbols, and folding ranges come back through the real LSP client stack.
const vscode = require('vscode');
const fs = require('fs');
const path = require('path');

const FIXDIR = process.env.SMOKE_FIXTURES;
const REPORT = process.env.SMOKE_REPORT;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Providers register asynchronously after the server initializes; retry.
async function poll(fn, what, tries = 50) {
  let last;
  for (let i = 0; i < tries; i++) {
    try {
      const v = await fn();
      if (v !== undefined && v !== null) return v;
      last = `empty result`;
    } catch (e) {
      last = e.message;
    }
    await sleep(200);
  }
  throw new Error(`timeout waiting for ${what}: ${last}`);
}

async function checkDoc(report, file, expects) {
  const uri = vscode.Uri.file(path.join(FIXDIR, file));
  const doc = await vscode.workspace.openTextDocument(uri);
  await vscode.window.showTextDocument(doc);
  const entry = { file, languageId: doc.languageId };

  const tokens = await poll(
    async () => {
      const t = await vscode.commands.executeCommand(
        'vscode.provideDocumentSemanticTokens',
        uri
      );
      return t && t.data && t.data.length ? t : undefined;
    },
    `${file} semantic tokens`
  );
  entry.semanticTokenInts = tokens.data.length;
  if (tokens.data.length % 5 !== 0) throw new Error(`${file}: token data not 5-aligned`);

  const legend = await vscode.commands.executeCommand(
    'vscode.provideDocumentSemanticTokensLegend',
    uri
  );
  entry.legendTypes = legend ? legend.tokenTypes.length : 0;

  const symbols = await poll(
    async () => {
      const s = await vscode.commands.executeCommand(
        'vscode.executeDocumentSymbolProvider',
        uri
      );
      return s && s.length ? s : undefined;
    },
    `${file} symbols`
  );
  entry.symbols = symbols.map((s) => s.name);
  for (const name of expects.symbols) {
    if (!entry.symbols.includes(name)) {
      throw new Error(`${file}: missing symbol ${name}, got ${entry.symbols}`);
    }
  }

  const folds = await poll(
    async () => {
      const f = await vscode.commands.executeCommand(
        'vscode.executeFoldingRangeProvider',
        uri
      );
      return f && f.length >= expects.minFolds ? f : undefined;
    },
    `${file} folding ranges`
  );
  entry.folds = folds.map((f) => [f.start, f.end]);

  // Exercise incremental sync + the delta path: edit, then re-request.
  const editor = vscode.window.activeTextEditor;
  const before = tokens.data.length;
  await editor.edit((b) =>
    b.insert(new vscode.Position(doc.lineCount, 0), '\n' + expects.append + '\n')
  );
  const after = await poll(
    async () => {
      const t = await vscode.commands.executeCommand(
        'vscode.provideDocumentSemanticTokens',
        uri
      );
      return t && t.data && t.data.length > before ? t : undefined;
    },
    `${file} tokens after edit`
  );
  entry.semanticTokenIntsAfterEdit = after.data.length;

  report.docs.push(entry);
}

exports.run = async function run() {
  const report = { pass: false, docs: [], error: null };
  try {
    await checkDoc(report, 'smoke.ttl', {
      symbols: ['ex:'],
      minFolds: 1,
      append: 'ex:added ex:p "new" .',
    });
    await checkDoc(report, 'smoke.rq', {
      symbols: ['ex:', 'SELECT'],
      minFolds: 1,
      append: '# trailing comment with nothing to parse\nORDER BY ?s',
    });
    await checkDoc(report, 'smoke.jsonld', {
      symbols: ['@context', '@id', 'name'],
      minFolds: 1,
      append: '{"@id": "http://example.org/y", "extra": 42}',
    });
    report.pass = true;
  } catch (e) {
    report.error = e.message;
  }
  fs.writeFileSync(REPORT, JSON.stringify(report, null, 2));
  if (!report.pass) throw new Error(report.error);
};
