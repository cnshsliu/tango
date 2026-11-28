import { readFile } from 'node:fs/promises';
import { dirname, isAbsolute, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { Tool } from '../types.js';
import { spawn } from 'node:child_process';
import { clientFor, forgetProbes, installCommand, searchDirs, serverFor, type LspClient } from '../lsp.js';
import { DETACHED, PATH_SEP, killProcessTree, shellInvocation } from '../shell.js';
import { error, text, truncate } from './registry.js';

const OPS = ['install', 'definition', 'references', 'hover', 'symbols', 'workspace_symbols', 'implementation', 'incoming_calls', 'outgoing_calls'] as const;
type Op = (typeof OPS)[number];

interface LspInput {
  op: Op;
  path: string;
  line?: number;
  symbol?: string;
  column?: number;
  query?: string;
}

const SYMBOL_KINDS = ['', 'file', 'module', 'namespace', 'package', 'class', 'method', 'property', 'field', 'constructor', 'enum', 'interface', 'function', 'variable', 'constant', 'string', 'number', 'boolean', 'array', 'object', 'key', 'null', 'enum member', 'struct', 'event', 'operator', 'type parameter'];

/**
 * 0-based character offset of the position to ask about: `column` (1-based) when given, else the
 * start of `symbol` on that line (whole-word match preferred), else the first non-space character.
 */
export function positionOn(lineText: string, symbol?: string, column?: number): number | undefined {
  if (column && column > 0) return Math.min(column - 1, lineText.length);
  if (symbol) {
    const esc = symbol.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const word = new RegExp(`(^|[^\\w$])(${esc})(?![\\w$])`).exec(lineText);
    if (word) return word.index + word[1].length;
    const i = lineText.indexOf(symbol);
    return i >= 0 ? i : undefined;
  }
  const m = /\S/.exec(lineText);
  return m ? m.index : 0;
}

const lineCache = new Map<string, string[]>();
async function lineOf(file: string, line0: number): Promise<string> {
  let lines = lineCache.get(file);
  if (!lines) {
    lines = (await readFile(file, 'utf8').catch(() => '')).split('\n');
    lineCache.set(file, lines);
  }
  return (lines[line0] ?? '').trim().slice(0, 160);
}

const show = (file: string, cwd: string) => {
  const r = relative(cwd, file);
  return r.startsWith('..') || isAbsolute(r) ? file : r;
};

/** Location | LocationLink | arrays of either → "path:line:col  source line", deduped. */
export async function formatLocations(result: any, cwd: string, limit = 100): Promise<string[]> {
  const list = Array.isArray(result) ? result : result ? [result] : [];
  const out: string[] = [];
  const seen = new Set<string>();
  for (const loc of list) {
    const uri = loc.targetUri ?? loc.uri;
    const range = loc.targetSelectionRange ?? loc.targetRange ?? loc.range;
    if (!uri?.startsWith('file:') || !range) continue;
    const file = fileURLToPath(uri);
    const key = `${file}:${range.start.line}:${range.start.character}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(`${show(file, cwd)}:${range.start.line + 1}:${range.start.character + 1}  ${await lineOf(file, range.start.line)}`);
    if (out.length >= limit) break;
  }
  return out;
}

/** DocumentSymbol tree or SymbolInformation list → indented outline lines. */
export function formatSymbols(result: any[], cwd: string, depth = 0, withPath = false): string[] {
  const out: string[] = [];
  const at = (s: any) => (s.selectionRange ?? s.range ?? s.location?.range)?.start.line ?? 0;
  // an outline reads top to bottom (servers often sort by name); workspace hits keep their ranking
  const list = withPath ? (result ?? []) : [...(result ?? [])].sort((a, b) => at(a) - at(b));
  for (const s of list) {
    const kind = SYMBOL_KINDS[s.kind] ?? 'symbol';
    const range = s.selectionRange ?? s.range ?? s.location?.range;
    const where = s.location?.uri && withPath ? `${show(fileURLToPath(s.location.uri), cwd)}:` : '';
    const line = range ? `${where}${range.start.line + 1}` : where;
    out.push(`${'  '.repeat(depth)}${kind} ${s.name}${s.containerName && withPath ? ` (in ${s.containerName})` : ''}  ${line}`);
    if (s.children?.length) out.push(...formatSymbols(s.children, cwd, depth + 1, withPath));
  }
  return out;
}

export function formatHover(result: any): string {
  const c = result?.contents;
  if (!c) return '';
  const part = (x: any): string => (typeof x === 'string' ? x : x?.value ?? '');
  return (Array.isArray(c) ? c.map(part) : [part(c)]).filter(Boolean).join('\n\n').trim();
}

export const lspTool: Tool<LspInput> = {
  name: 'lsp',
  // only install changes the machine (plan mode blocks it; the gate always asks)
  writeEffect: (input) => input?.op === 'install',
  description:
    'Ask a language server about code: definition, references, hover (type and docs), symbols (outline of a file), ' +
    'workspace_symbols (find a symbol by name across the project), implementation, incoming_calls / outgoing_calls. ' +
    'Give path and line (1-based) plus the symbol name on that line (or column). More precise than grep for ' +
    '"where is X defined / who calls X". TypeScript/JavaScript, Python, Rust, Go, C/C++ when their server is installed; ' +
    'the first call per project starts the server and can take a few seconds. When the server is missing, the error ' +
    'names an install command: op "install" with the same path runs it after the user approves.',
  input: {
    type: 'object',
    properties: {
      op: { type: 'string', enum: [...OPS] },
      path: { type: 'string', description: 'File (for workspace_symbols: any file of the project)' },
      line: { type: 'number', description: '1-based line (not needed for symbols / workspace_symbols)' },
      symbol: { type: 'string', description: 'The identifier on that line to ask about' },
      column: { type: 'number', description: '1-based column, instead of symbol' },
      query: { type: 'string', description: 'workspace_symbols: name to search' }
    },
    required: ['op', 'path']
  },
  async execute(input, ctx) {
    const { op } = input;
    if (!OPS.includes(op)) return error(`op must be one of: ${OPS.join(', ')}`);
    const file = resolve(ctx.cwd, input.path);
    if (op === 'install') return install(file, ctx.signal);
    const client = clientFor(file, ctx.cwd);
    if ('error' in client) return error(client.error);
    try {
      lineCache.clear();
      const uri = await client.open(file);
      if (op === 'symbols') {
        const lines = formatSymbols(await client.request('textDocument/documentSymbol', { textDocument: { uri } }), ctx.cwd);
        return text(lines.length ? truncate(lines.join('\n')) : 'no symbols');
      }
      if (op === 'workspace_symbols') {
        if (!input.query) return error('workspace_symbols needs query');
        const ask = async () => formatSymbols(await client.request('workspace/symbol', { query: input.query }), ctx.cwd, 0, true);
        const lines = await warmRetry(client, ask, ctx.signal);
        return text(lines.length ? truncate(lines.slice(0, 100).join('\n')) : `no symbol matches "${input.query}"`);
      }
      if (!input.line || input.line < 1) return error(`${op} needs line (1-based)`);
      const src = (await readFile(file, 'utf8')).split('\n');
      const lineText = src[input.line - 1];
      if (lineText === undefined) return error(`line ${input.line} is past the end of ${input.path} (${src.length} lines)`);
      const character = positionOn(lineText, input.symbol, input.column);
      if (character === undefined) return error(`"${input.symbol}" is not on line ${input.line}: ${lineText.trim().slice(0, 120)}`);
      const pos = { textDocument: { uri }, position: { line: input.line - 1, character } };
      return await warmRetry(client, () => answer(client, op, pos, ctx.cwd), ctx.signal, (r: { content: { text: string }[]; isError?: boolean }) => !r.isError && notReady(op, r.content[0].text));
    } catch (e: any) {
      return error(e.message);
    }
  }
};

/** An import / re-export line: TypeScript answers a definition with it until the project is loaded. */
const IMPORT_LINE = /^\s*(import\b|export\s+(\*|\{[^}]*\})\s+from\b)/;

/** An answer a server still loading the project gives: nothing, or a definition that only reaches an import. */
export function notReady(op: string, out: string): boolean {
  if (/^no /.test(out)) return true;
  if (op !== 'definition') return false;
  return out.split('\n').every((l) => IMPORT_LINE.test(l.slice(l.indexOf('  ') + 2)));
}

/** Run the install recipe for this file's language server (the permission gate showed it first). */
async function install(file: string, signal: AbortSignal) {
  const def = serverFor(file);
  if (!def) return error(`no language server for this file type`);
  const cmd = installCommand(def);
  if (!cmd) return error(`no installer found for the ${def.language} server (tried ${def.install.map((r) => r.needs).join(', ')}); the user has to install it`);
  const { out, code } = await new Promise<{ out: string; code: number | null }>((done) => {
    // the installer itself may live in a toolchain dir that PATH lacks (rustup in ~/.cargo/bin)
    const env = { ...process.env, PATH: searchDirs().join(PATH_SEP) };
    const shell = shellInvocation(cmd);
    const child = spawn(shell.file, shell.args, { stdio: ['ignore', 'pipe', 'pipe'], detached: DETACHED, windowsHide: true, env });
    let out = '';
    child.stdout.on('data', (b: Buffer) => (out = (out + b.toString()).slice(-6000)));
    child.stderr.on('data', (b: Buffer) => (out = (out + b.toString()).slice(-6000)));
    const kill = () => killProcessTree(child.pid);
    signal.addEventListener('abort', kill, { once: true });
    const timer = setTimeout(kill, 10 * 60_000);
    child.on('error', (e) => done({ out: e.message, code: -1 }));
    child.on('close', (code) => {
      clearTimeout(timer);
      signal.removeEventListener('abort', kill);
      done({ out, code });
    });
  });
  forgetProbes(); // the server may run now
  const tail = out.trim().split('\n').slice(-15).join('\n');
  if (code !== 0) return error(`\`${cmd}\` failed (exit ${code}):\n${tail}`);
  const now = clientFor(file, dirname(file));
  if ('error' in now) return error(`\`${cmd}\` finished, but the server still isn't found (searched: ${searchDirs().join(PATH_SEP)}).\n${tail}`);
  return text(`installed with \`${cmd}\`; the ${def.language} language server is ready.`);
}

/**
 * A server that just started is still loading the project and answers "nothing" for a few
 * seconds. While it is under 20 s old, an empty answer is retried every 1.5 s, up to ~12 s.
 */
export async function warmRetry<T>(client: { startedAt: number }, ask: () => Promise<T>, signal: AbortSignal, empty: (r: T) => boolean = (r: any) => !r?.length): Promise<T> {
  let r = await ask();
  for (let i = 0; i < 8 && empty(r) && Date.now() - client.startedAt < 20_000 && !signal.aborted; i++) {
    await new Promise((ok) => setTimeout(ok, 1500));
    r = await ask();
  }
  return r;
}

async function answer(client: LspClient, op: Op, pos: any, cwd: string) {
  const none = (what: string) => text(`no ${what} found (the server may still be indexing; retry, or fall back to grep)`);
  switch (op) {
    case 'hover': {
      const h = formatHover(await client.request('textDocument/hover', pos));
      return h ? text(truncate(h)) : none('hover info');
    }
    case 'definition':
    case 'implementation':
    case 'references': {
      const method = op === 'references' ? 'textDocument/references' : `textDocument/${op}`;
      const params = op === 'references' ? { ...pos, context: { includeDeclaration: true } } : pos;
      const result = await client.request(method, params);
      const lines = await formatLocations(result, cwd);
      return lines.length ? text(lines.join('\n')) : none(op);
    }
    default: {
      const items = await client.request('textDocument/prepareCallHierarchy', pos);
      if (!items?.length) return none('call hierarchy item');
      const method = op === 'incoming_calls' ? 'callHierarchy/incomingCalls' : 'callHierarchy/outgoingCalls';
      const calls = (await client.request(method, { item: items[0] })) ?? [];
      const lines: string[] = [];
      for (const c of calls.slice(0, 100)) {
        if (op === 'incoming_calls') {
          // each call site, labelled with the calling function (the file name for top-level code)
          const sites = c.fromRanges?.length ? c.fromRanges : [c.from.selectionRange];
          for (const loc of await formatLocations(sites.map((range: any) => ({ uri: c.from.uri, range })), cwd))
            lines.push(`${c.from.name}  ${loc}`);
        } else {
          const [loc] = await formatLocations({ uri: c.to.uri, range: c.to.selectionRange }, cwd);
          if (loc) lines.push(`${c.to.name}  ${loc}`);
        }
      }
      return lines.length ? text(lines.join('\n')) : none(op.replace('_', ' '));
    }
  }
}
