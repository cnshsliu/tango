/**
 * The /doctor checks, shared by serve (`/doctor`, `/debug-zip`) and `tango doctor`. Read-only
 * apart from a scratch directory created and removed under the sessions dir. Never prints or
 * returns a secret: key checks say whether one resolves, redaction says how many values are
 * watched, the proxy line never carries the credentials of a proxy URL.
 */
import { accessSync, constants } from 'node:fs';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { keyProblem, sessionsDir } from './config.js';
import { proxyStatus } from './env-proxy.js';
import { installCommand, resolveCommand, SERVERS } from './lsp.js';
import type { ModelSpec } from './types.js';

export interface DoctorCheck {
  name: string;
  ok: boolean;
  detail: string;
}

export interface DoctorInput {
  /** the directory the session runs (or would run) in */
  cwd: string;
  /** the resolved model: its key is what gets checked */
  model: ModelSpec;
  /** the merged config, project layers included */
  config: { redactSecrets?: boolean; lspDiagnostics?: boolean };
}

export async function runDoctor({ cwd, model, config }: DoctorInput): Promise<DoctorCheck[]> {
  const checks: DoctorCheck[] = [];
  const add = (name: string, ok: boolean, detail = '') => checks.push({ name, ok, detail });

  const gitOk = (() => {
    try {
      accessSync(join(cwd, '.git'), constants.F_OK);
      return true;
    } catch {
      return false;
    }
  })();
  add('git', gitOk, gitOk ? 'repo detected — time travel checkpoints active' : 'not a git repo — tar fallback snapshots');

  let homeWritable = false;
  try {
    // the sessions dir exists once a session was opened; `tango doctor` on a fresh install
    // has not, and must not report that as unwritable
    await mkdir(sessionsDir(), { recursive: true });
    const d = await mkdtemp(join(sessionsDir(), 'doctor-'));
    await rm(d, { recursive: true, force: true });
    homeWritable = true;
  } catch {}
  add('TANGO_HOME writable', homeWritable);
  add('sessions dir', homeWritable, sessionsDir());

  // keyProblem's text starts with the id; the line has it already
  const issue = keyProblem(model);
  const why = issue?.startsWith(`${model.id}: `) ? issue.slice(model.id.length + 2) : issue;
  add('model key', !issue, issue ? `${model.id}: no api key configured (${why})` : `${model.id} ready`);

  // how http(s)_proxy is being handled in this process
  add('proxy', true, proxyStatus());
  add('node', true, process.version);

  // tool-output secret redaction: whether it is on, and how many values it watches
  const { knownSecrets } = await import('./redact.js');
  const watched = config.redactSecrets === false ? [] : await knownSecrets().catch(() => []);
  add(
    'secret redaction',
    config.redactSecrets !== false,
    config.redactSecrets === false ? 'off (redactSecrets: false)' : `${watched.length} value${watched.length === 1 ? '' : 's'} watched`
  );
  add(
    'lsp diagnostics',
    config.lspDiagnostics !== false,
    config.lspDiagnostics === false ? 'off (lspDiagnostics: false)' : 'on · after each edit of a file whose server is installed'
  );
  // language servers for the lsp tool: what runs, and how to get what doesn't
  for (const def of SERVERS) {
    const found = resolveCommand(def, cwd);
    const how = installCommand(def);
    add(
      `lsp ${def.language}`,
      !!found,
      found ? found[0] : how ? `not installed · ${how}` : `not installed · needs ${def.install.map((r) => r.needs).join(' or ')}`
    );
  }
  return checks;
}

/** `tango doctor`: one line per check, ✓ or ✗, then the detail. */
export function formatDoctor(checks: DoctorCheck[]): string {
  return checks.map((c) => `${c.ok ? '✓' : '✗'} ${c.name}${c.detail ? `  ${c.detail}` : ''}`).join('\n');
}
