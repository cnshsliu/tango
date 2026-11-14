import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import type { EventBus } from './events.js';
import type { ToolContext } from './types.js';

/** Per-extension project checkers: marker files to find the project root, then the command to run. */
const CHECKERS: Record<string, { markers: string[]; run: (root: string) => [string, string[]] | undefined }> = {
  '.ts': {
    markers: ['tsconfig.json'],
    run: (root) => (existsSync(join(root, 'node_modules/.bin/tsc')) ? [join(root, 'node_modules/.bin/tsc'), ['-p', root, '--noEmit']] : undefined)
  },
  '.tsx': {
    markers: ['tsconfig.json'],
    run: (root) => (existsSync(join(root, 'node_modules/.bin/tsc')) ? [join(root, 'node_modules/.bin/tsc'), ['-p', root, '--noEmit']] : undefined)
  },
  '.py': {
    markers: ['pyproject.toml', 'setup.py', 'requirements.txt'],
    run: (root) => {
      if (existsSync(join(root, '.venv/bin/pyright'))) return [join(root, '.venv/bin/pyright'), ['--outputjson', root]];
      if (existsSync(join(root, 'node_modules/.bin/pyright'))) return [join(root, 'node_modules/.bin/pyright'), ['--outputjson', root]];
      if (existsSync(join(root, '.venv/bin/ruff'))) return [join(root, '.venv/bin/ruff'), ['check', root]];
      return undefined;
    }
  },
  '.go': { markers: ['go.mod'], run: () => ['go', ['vet', './...']] },
  '.rs': { markers: ['Cargo.toml'], run: () => ['cargo', ['check', '--message-format', 'short']] }
};

/** Nearest directory holding one of the markers; falls back to the file's own directory. */
export function projectRoot(file: string, ext: string): string | undefined {
  const checker = CHECKERS[ext];
  if (!checker) return undefined;
  let dir = dirname(file);
  while (true) {
    if (checker.markers.some((m) => existsSync(join(dir, m)))) return dir;
    const parent = dirname(dir);
    if (parent === dir) return;
    dir = parent;
  }
}

function run(cmd: string, args: string[], cwd: string, signal: AbortSignal, timeoutMs = 60_000): Promise<{ out: string; code: number | null }> {
  return new Promise((done) => {
    const child = spawn(cmd, args, { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    const cap = () => { if (out.length < 40_000) return; child.kill('SIGKILL'); };
    child.stdout.on('data', (b: Buffer) => { out += b.toString(); cap(); });
    child.stderr.on('data', (b: Buffer) => { out += b.toString(); cap(); });
    const kill = () => child.kill('SIGKILL');
    const timer = setTimeout(kill, timeoutMs);
    signal.addEventListener('abort', kill, { once: true });
    child.on('error', (e) => { clearTimeout(timer); signal.removeEventListener('abort', kill); done({ out: `cannot run ${cmd}: ${e.message}`, code: -1 }); });
    child.on('close', (code) => { clearTimeout(timer); signal.removeEventListener('abort', kill); done({ out, code }); });
  });
}

/** Compact plain-text diagnostics; empty string when the file looks clean. */
export async function diagnosticsFor(path: string, ctx: ToolContext): Promise<string> {
  const file = resolve(ctx.cwd, path);
  const ext = file.slice(file.lastIndexOf('.'));
  const checker = CHECKERS[ext];
  if (!checker) return '';
  const root = projectRoot(file, ext);
  if (!root) return '';
  const cmd = checker.run(root);
  if (!cmd) return ''; // project has no checker installed — stay silent, don't nag

  if (ext === '.py' && cmd[0].endsWith('pyright')) {
    const r = await run(cmd[0], ['--outputjson', root], root, ctx.signal);
    try {
      const diag = JSON.parse(r.out.slice(r.out.indexOf('{'))).diagnostics ?? [];
      const errs = diag.filter((d: any) => d.severity === 'error' && resolve(d.file ?? '') === file);
      if (!errs.length) return r.code === 0 ? '' : clip(r.out);
      return errs.map((d: any) => `${file.split('/').pop()}:${d.range?.start?.line ?? '?'} ${d.message}`).join('\n');
    } catch {
      return r.code === 0 ? '' : clip(r.out);
    }
  }
  if (ext === '.rs') {
    const r = await run(cmd[0], [...cmd[1], '--quiet'], root, ctx.signal);
    const lines = r.out.split('\n').filter((l) => l.includes('error'));
    return r.code === 0 ? '' : clip(lines.join('\n') || r.out);
  }
  const r = await run(cmd[0], ['-p', root, '--noEmit', '--pretty', 'false'], root, ctx.signal);
  if (r.code === 0) return '';
  return clip(r.out.split('\n').map((l) => l.replace(/^\.\.\.\//, '').replace(/^.*?\/(?=[\w.\-]+\(\d)/, '')).join('\n') || r.out);
}

const clip = (s: string) => {
  const t = s.trim();
  return t.length > 2_000 ? t.slice(0, 2_000) + '\n…(diagnostics clipped)' : t;
};

/**
 * After write/edit of a source file, run the project's own checker and append
 * the errors to the tool result, so the model sees a broken edit immediately.
 */
export function installDiagnostics(events: EventBus, cwd: string): () => void {
  return events.on('after_tool', async ({ call, result }) => {
    if ((call.name !== 'write' && call.name !== 'edit') || result.isError) return;
    const path = (call.input as { path?: string } | undefined)?.path;
    if (!path) return;
    const ctx: ToolContext = { cwd, signal: new AbortController().signal };
    let text: string;
    try {
      text = await diagnosticsFor(path, ctx);
    } catch {
      return;
    }
    if (!text) return;
    result.content.push({ type: 'text', text: `[diagnostics after this edit]\n${text}` });
  });
}
