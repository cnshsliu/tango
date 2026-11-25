/**
 * A small Language Server Protocol client for the `lsp` tool: JSON-RPC 2.0 over stdio with
 * Content-Length framing. One server process per (language, project root), started on first use
 * and kept until the tango process exits. Only what the tool needs: initialize, didOpen, and
 * request/response; server→client requests get an empty result so servers never stall on us.
 */
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { dirname, extname, join } from 'node:path';
import { pathToFileURL } from 'node:url';

export interface ServerDef {
  /** language id sent in didOpen */
  language: string;
  exts: string[];
  /** files whose directory is the project root, nearest first */
  markers: string[];
  /** candidate commands, first one found wins; relative paths are tried under the root */
  commands: [string, string[]][];
}

export const SERVERS: ServerDef[] = [
  {
    language: 'typescript',
    exts: ['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.mjs', '.cjs'],
    markers: ['tsconfig.json', 'jsconfig.json', 'package.json'],
    commands: [
      ['node_modules/.bin/typescript-language-server', ['--stdio']],
      ['typescript-language-server', ['--stdio']]
    ]
  },
  {
    language: 'python',
    exts: ['.py'],
    markers: ['pyproject.toml', 'setup.py', 'setup.cfg', 'requirements.txt'],
    commands: [
      ['.venv/bin/pyright-langserver', ['--stdio']],
      ['node_modules/.bin/pyright-langserver', ['--stdio']],
      ['pyright-langserver', ['--stdio']]
    ]
  },
  { language: 'rust', exts: ['.rs'], markers: ['Cargo.toml'], commands: [['rust-analyzer', []]] },
  { language: 'go', exts: ['.go'], markers: ['go.mod'], commands: [['gopls', []]] },
  {
    language: 'c',
    exts: ['.c', '.h', '.cc', '.cpp', '.hpp', '.m'],
    markers: ['compile_commands.json', 'CMakeLists.txt', 'Makefile'],
    commands: [['clangd', []]]
  }
];

export const serverFor = (file: string) => SERVERS.find((s) => s.exts.includes(extname(file).toLowerCase()));

/** Nearest ancestor directory holding a marker; else the file's directory. */
export function findRoot(file: string, markers: string[], stop?: string): string {
  let dir = dirname(file);
  while (true) {
    if (markers.some((m) => existsSync(join(dir, m)))) return dir;
    const parent = dirname(dir);
    if (parent === dir || dir === stop) return dirname(file);
    dir = parent;
  }
}

/** The first command that exists: a relative path under the root, or a name on PATH. */
export function resolveCommand(def: ServerDef, root: string): [string, string[]] | undefined {
  for (const [cmd, args] of def.commands) {
    if (cmd.includes('/')) {
      // look in the root and its ancestors (monorepo node_modules)
      let dir = root;
      while (true) {
        if (existsSync(join(dir, cmd))) return [join(dir, cmd), args];
        const parent = dirname(dir);
        if (parent === dir) break;
        dir = parent;
      }
    } else {
      for (const p of (process.env.PATH ?? '').split(':')) if (p && existsSync(join(p, cmd))) return [join(p, cmd), args];
    }
  }
  return undefined;
}

type Pending = { resolve: (v: any) => void; reject: (e: Error) => void; timer: NodeJS.Timeout };

export class LspClient {
  private child: ChildProcessWithoutNullStreams;
  private buf = Buffer.alloc(0);
  private seq = 0;
  private pending = new Map<number, Pending>();
  private opened = new Map<string, number>();
  private ready: Promise<void>;
  private stderr = '';
  exited = false;
  readonly startedAt = Date.now();

  constructor(readonly def: ServerDef, readonly root: string, cmd: [string, string[]]) {
    this.child = spawn(cmd[0], cmd[1], { cwd: root, stdio: ['pipe', 'pipe', 'pipe'] });
    this.child.on('error', (e) => this.fail(new Error(`${def.language} language server: ${e.message}`)));
    this.child.on('exit', (code) => this.fail(new Error(`${def.language} language server exited (${code}): ${this.stderr.slice(-500)}`)));
    this.child.stderr.on('data', (b: Buffer) => (this.stderr = (this.stderr + b.toString()).slice(-4000)));
    this.child.stdout.on('data', (b: Buffer) => this.onData(b));
    // a dead pipe must not crash tango
    this.child.stdin.on('error', () => {});
    this.ready = this.initialize();
  }

  private fail(e: Error) {
    this.exited = true;
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(e);
    }
    this.pending.clear();
  }

  private write(msg: object) {
    const body = Buffer.from(JSON.stringify({ jsonrpc: '2.0', ...msg }), 'utf8');
    this.child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.child.stdin.write(body);
  }

  private onData(chunk: Buffer) {
    this.buf = Buffer.concat([this.buf, chunk]);
    while (true) {
      const sep = this.buf.indexOf('\r\n\r\n');
      if (sep < 0) return;
      const m = /Content-Length:\s*(\d+)/i.exec(this.buf.subarray(0, sep).toString('ascii'));
      if (!m) {
        this.buf = this.buf.subarray(sep + 4); // garbage header: skip it
        continue;
      }
      const len = Number(m[1]);
      if (this.buf.length < sep + 4 + len) return;
      const body = this.buf.subarray(sep + 4, sep + 4 + len).toString('utf8');
      this.buf = this.buf.subarray(sep + 4 + len);
      let msg: any;
      try {
        msg = JSON.parse(body);
      } catch {
        continue;
      }
      this.onMessage(msg);
    }
  }

  private onMessage(msg: any) {
    if (msg.id !== undefined && msg.method) {
      // server → client request (workspace/configuration, window/workDoneProgress/create, …)
      const result = msg.method === 'workspace/configuration' ? (msg.params?.items ?? []).map(() => null) : null;
      this.write({ id: msg.id, result });
      return;
    }
    if (msg.id === undefined) return; // notifications (diagnostics, progress): not needed
    const p = this.pending.get(msg.id);
    if (!p) return;
    this.pending.delete(msg.id);
    clearTimeout(p.timer);
    if (msg.error) p.reject(new Error(`${this.def.language} language server: ${msg.error.message ?? JSON.stringify(msg.error)}`));
    else p.resolve(msg.result);
  }

  private raw(method: string, params: unknown, timeoutMs = 30_000): Promise<any> {
    if (this.exited) return Promise.reject(new Error(`${this.def.language} language server is not running: ${this.stderr.slice(-300)}`));
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${this.def.language} language server: ${method} timed out after ${timeoutMs / 1000}s`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.write({ id, method, params });
    });
  }

  private async initialize() {
    const rootUri = pathToFileURL(this.root).href;
    await this.raw(
      'initialize',
      {
        processId: process.pid,
        rootUri,
        workspaceFolders: [{ uri: rootUri, name: this.root.split('/').pop() }],
        capabilities: {
          textDocument: {
            definition: { linkSupport: true },
            references: {},
            hover: { contentFormat: ['markdown', 'plaintext'] },
            documentSymbol: { hierarchicalDocumentSymbolSupport: true },
            implementation: { linkSupport: true },
            callHierarchy: {}
          },
          workspace: { symbol: {}, workspaceFolders: true, configuration: true }
        }
      },
      60_000
    );
    this.write({ method: 'initialized', params: {} });
  }

  /** didOpen once per file (re-sent when the file changed on disk since). */
  async open(file: string): Promise<string> {
    await this.ready;
    const uri = pathToFileURL(file).href;
    const text = await readFile(file, 'utf8');
    const prev = this.opened.get(uri);
    const version = (prev ?? 0) + 1;
    if (prev === undefined) {
      this.write({ method: 'textDocument/didOpen', params: { textDocument: { uri, languageId: this.def.language, version, text } } });
    } else {
      this.write({ method: 'textDocument/didChange', params: { textDocument: { uri, version }, contentChanges: [{ text }] } });
    }
    this.opened.set(uri, version);
    return uri;
  }

  async request(method: string, params: unknown, timeoutMs?: number): Promise<any> {
    await this.ready;
    return this.raw(method, params, timeoutMs);
  }

  stop() {
    if (this.exited) return;
    this.exited = true;
    try {
      this.write({ id: ++this.seq, method: 'shutdown', params: null });
      this.write({ method: 'exit', params: null });
    } catch {}
    setTimeout(() => this.child.kill('SIGKILL'), 1000).unref();
  }
}

const clients = new Map<string, LspClient>();

/** The running client for this file's language and project, started on first use. */
export function clientFor(file: string, cwd: string): LspClient | { error: string } {
  const def = serverFor(file);
  if (!def) return { error: `no language server for ${extname(file) || 'this file type'} (supported: ${SERVERS.flatMap((s) => s.exts).join(' ')})` };
  const root = findRoot(file, def.markers);
  const key = `${def.language}\0${root}`;
  const live = clients.get(key);
  if (live && !live.exited) return live;
  const cmd = resolveCommand(def, root) ?? resolveCommand(def, cwd);
  if (!cmd) {
    const names = def.commands.map(([c]) => c.split('/').pop()).filter((v, i, a) => a.indexOf(v) === i);
    return { error: `no ${def.language} language server found (looked for ${names.join(', ')} under ${root} and on PATH). Install one, or use grep.` };
  }
  const client = new LspClient(def, root, cmd);
  clients.set(key, client);
  return client;
}

export function stopAllLsp() {
  for (const c of clients.values()) c.stop();
  clients.clear();
}
process.once('exit', stopAllLsp);
