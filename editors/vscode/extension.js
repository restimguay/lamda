// Type-checks lamda files with `lamda check` and adds a Run command.
const vscode = require('vscode');
const { execFile } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const ERROR_LINE = /^lamda: (.*?):(\d+): (.*)$/;
const ERROR_FILE = /^lamda: (.*?): (.*)$/;

let diagnostics;
const pending = new Map();
const running = new Map();

function activate(context) {
  diagnostics = vscode.languages.createDiagnosticCollection('lamda');
  context.subscriptions.push(diagnostics);

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(check),
    vscode.workspace.onDidSaveTextDocument(check),
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (vscode.workspace.getConfiguration('lamda').get('checkOnType')) {
        schedule(e.document);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((doc) => diagnostics.delete(doc.uri)),
    vscode.commands.registerCommand('lamda.run', run),
  );
  vscode.workspace.textDocuments.forEach(check);
}

function compilerPath(doc) {
  const configured = vscode.workspace.getConfiguration('lamda').get('compilerPath');
  if (configured) {
    return configured;
  }
  const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
  if (folder) {
    const local = path.join(folder.uri.fsPath, 'target', 'release', 'lamda');
    if (fs.existsSync(local)) {
      return local;
    }
  }
  return 'lamda';
}

function schedule(doc) {
  if (doc.languageId !== 'lamda') {
    return;
  }
  clearTimeout(pending.get(doc.uri.toString()));
  pending.set(doc.uri.toString(), setTimeout(() => check(doc), 300));
}

function check(doc) {
  // Skip virtual documents such as git diffs of a .lm file.
  if (doc.languageId !== 'lamda' || !['file', 'untitled'].includes(doc.uri.scheme)) {
    return;
  }
  const key = doc.uri.toString();
  pending.delete(key);
  running.get(key)?.kill();

  // Check the editor's contents, which may be unsaved, through a temp file.
  const tmp = path.join(os.tmpdir(), `lamda-check-${process.pid}-${Buffer.from(key).toString('hex').slice(-16)}.lm`);
  fs.writeFileSync(tmp, doc.getText());
  const version = doc.version;

  const child = execFile(compilerPath(doc), ['check', tmp], (err, _stdout, stderr) => {
    if (running.get(key) === child) {
      running.delete(key);
    }
    fs.rm(tmp, { force: true }, () => {});
    if (doc.isClosed || doc.version !== version) {
      return;
    }
    if (err && err.code === 'ENOENT') {
      diagnostics.set(doc.uri, [atLine(doc, 0, 'lamda compiler not found: build it with `cargo build --release` or set `lamda.compilerPath`', vscode.DiagnosticSeverity.Warning)]);
      return;
    }
    diagnostics.set(doc.uri, parse(doc, stderr));
  });
  running.set(key, child);
}

function parse(doc, stderr) {
  const result = [];
  for (const line of stderr.split('\n')) {
    let m = ERROR_LINE.exec(line);
    if (m) {
      result.push(atLine(doc, Number(m[2]) - 1, m[3]));
    } else if ((m = ERROR_FILE.exec(line))) {
      result.push(atLine(doc, 0, m[2]));
    }
  }
  return result;
}

// The compiler reports lines, not columns, so underline the line's code.
function atLine(doc, line, message, severity = vscode.DiagnosticSeverity.Error) {
  const n = Math.min(Math.max(line, 0), doc.lineCount - 1);
  const text = doc.lineAt(n);
  const start = text.firstNonWhitespaceCharacterIndex;
  const end = Math.max(text.range.end.character, start + 1);
  const diagnostic = new vscode.Diagnostic(new vscode.Range(n, start, n, end), message, severity);
  diagnostic.source = 'lamda';
  return diagnostic;
}

async function run() {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== 'lamda') {
    vscode.window.showErrorMessage('Open a .lm file to run it.');
    return;
  }
  const doc = editor.document;
  if (doc.isUntitled) {
    vscode.window.showErrorMessage('Save the file before running it.');
    return;
  }
  await doc.save();

  let terminal = vscode.window.terminals.find((t) => t.name === 'lamda');
  if (!terminal) {
    terminal = vscode.window.createTerminal('lamda');
  }
  terminal.show(true);
  terminal.sendText(`${quote(compilerPath(doc))} run ${quote(doc.uri.fsPath)}`);
}

function quote(s) {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

function deactivate() {}

module.exports = { activate, deactivate };
