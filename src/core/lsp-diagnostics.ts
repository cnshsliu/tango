/**
 * LSP diagnostics for the TUI: after write/edit of a file a language server knows, hand it the
 * new text; whatever the server then publishes for that file is emitted as a `diagnostics`
 * event, which serve turns into a frame (the TUI shows a count in the status bar and `/problems`
 * lists them). Silent when no server is installed — nothing is installed or asked for — and off
 * with `"lspDiagnostics": false`. The model keeps getting the project checker's output
 * (diagnostics.ts), which also covers the files an edit breaks elsewhere.
 */
import { realpathSync } from 'node:fs';
import { isAbsolute, relative, resolve } from 'node:path';
import type { EventBus } from './events.js';
import { clientFor, serverFor, type LspClient } from './lsp.js';
import type { LspProblem } from './types.js';

const SEVERITY: Record<number, LspProblem['severity']> = { 1: 'error', 2: 'warning', 3: 'info', 4: 'hint' };
const RANK: Record<LspProblem['severity'], number> = { error: 0, warning: 1, info: 2, hint: 3 };
const MAX_PER_FILE = 200;

/** Raw LSP Diagnostics → problems, errors first then by position; at most 200. */
export function toProblems(raw: any[]): LspProblem[] {
  const out: LspProblem[] = [];
  for (const d of raw ?? []) {
    if (!d || typeof d !== 'object') continue;
    const message = String(d.message ?? '').replace(/\s+/g, ' ').trim().slice(0, 400);
    if (!message) continue;
    const severity = SEVERITY[d.severity] ?? 'error';
    const code = d.code === undefined || d.code === null ? undefined : typeof d.code === 'object' ? d.code.value : String(d.code);
    out.push({
      line: Math.max(1, (d.range?.start?.line ?? 0) + 1),
      col: Math.max(1, (d.range?.start?.character ?? 0) + 1),
      severity,
      message,
      ...(code ? { code } : {}),
      ...(d.source ? { source: String(d.source) } : {})
    });
  }
  return out
    .sort((a, b) => RANK[a.severity] - RANK[b.severity] || a.line - b.line || a.col - b.col)
    .slice(0, MAX_PER_FILE);
}

/**
 * Emit `diagnostics` for every file a language server reports on after tango edited it. Only
 * files inside the working directory are forwarded.
 */
export function installLspDiagnostics(events: EventBus, cwd: string | (() => string), enabled: () => boolean = () => true): () => void {
  const cwdNow = () => resolve(typeof cwd === 'function' ? cwd() : cwd);
  const watched = new Set<LspClient>();
  // servers may name a file by its real path while cwd was reached through a symlink
  const real = (p: string) => {
    try {
      return realpathSync(p);
    } catch {
      return p;
    }
  };
  const forward = (file: string, raw: any[]) => {
    const rel = relative(real(cwdNow()), real(file));
    if (rel.startsWith('..') || isAbsolute(rel)) return;
    events.emit('diagnostics', { file: rel, problems: toProblems(raw) }).catch(() => {});
  };
  return events.on('after_tool', ({ call, result }) => {
    if ((call.name !== 'write' && call.name !== 'edit') || result.isError || !enabled()) return;
    const path = (call.input as { path?: string } | undefined)?.path;
    if (!path || !serverFor(path)) return;
    const root = cwdNow();
    const file = resolve(root, path);
    const client = clientFor(file, root);
    if ('error' in client) return; // no server installed: stay silent, install nothing
    if (!watched.has(client)) {
      client.onDiagnostics = forward;
      watched.add(client);
    }
    client.open(file).catch(() => {});
  });
}
