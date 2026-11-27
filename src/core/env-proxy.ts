import { spawn } from 'node:child_process';
import { constants as osConstants } from 'node:os';
import * as http from 'node:http';

/**
 * Node's fetch ignores `http(s)_proxy` unless told to; users behind a proxy need every model
 * request to go through it. There are two ways to tell it, and which one exists depends on the Node
 * version:
 *
 *   Node 25+        `http.setGlobalProxyFromEnv()` does it in place, at any time.
 *   Node 22.21+     `NODE_USE_ENV_PROXY=1` (or `--use-env-proxy`), read once when node starts.
 *   Node 24+           tango cannot switch it on from inside the process, so it starts itself once
 *                      more with the switch on: the copy that stays is a small launcher that passes
 *                      its stdio and signals on and exits with the relaunched copy's status.
 *   older           nothing: the variables are ignored, and tango says so once.
 *
 * `no_proxy` is honoured either way, so a local model (Ollama, a local gateway) still goes direct.
 * `all_proxy` is not read by node.
 */

/** Read once by node at startup; on, every fetch and http request honours the proxy variables. */
export const ENV_PROXY_SWITCH = 'NODE_USE_ENV_PROXY';
/** Set in the relaunched copy: the launcher's pid, so the copy can notice being orphaned. */
export const LAUNCHER_PID = 'TANGO_ENV_PROXY_LAUNCHER';
const PROXY_VARS = ['HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy'];

export type ProxyPlan = 'off' | 'runtime' | 'relaunch' | 'unsupported';

/** The first Node versions that read NODE_USE_ENV_PROXY (node 22.21.0 and 24.0.0 release notes). */
export function nodeReadsEnvProxy(version: string): boolean {
  const [major, minor] = version.split('.').map(Number);
  return major >= 24 || (major === 22 && minor >= 21);
}

export function proxyPlan(
  env: NodeJS.ProcessEnv = process.env,
  nodeVersion = process.versions.node,
  canSetAtRuntime = typeof (http as any).setGlobalProxyFromEnv === 'function'
): ProxyPlan {
  if (env[ENV_PROXY_SWITCH]) return 'off'; // on already (this may be the relaunched copy)
  if (!PROXY_VARS.some((k) => env[k])) return 'off';
  if (canSetAtRuntime) return 'runtime';
  return nodeReadsEnvProxy(nodeVersion) ? 'relaunch' : 'unsupported';
}

/**
 * Act on the plan at startup. Resolves to the exit code when this process turned into the
 * launcher of a relaunched copy (the caller should then do nothing else), otherwise to undefined.
 */
export function applyEnvProxy(plan: ProxyPlan = proxyPlan(process.env)): Promise<number | undefined> {
  if (plan === 'runtime') (http as any).setGlobalProxyFromEnv();
  if (plan === 'unsupported') {
    console.error(`⚠ http(s)_proxy is set, but Node ${process.versions.node} ignores it (22.21+ or 24+ needed): requests go out directly`);
  }
  if (plan !== 'relaunch') return Promise.resolve(undefined);
  return relaunch();
}

function relaunch(): Promise<number> {
  return new Promise((resolve) => {
    const args = [...process.execArgv];
    // the switch announces itself as experimental on start; serve shows stderr lines in the chat
    const quietens = process.env.NODE_NO_WARNINGS || /--no-warnings|--disable-warning=(UNDICI-EHPA|ExperimentalWarning|Experimental)\b/.test(process.env.NODE_OPTIONS ?? '') || args.some((a) => a === '--no-warnings' || a.startsWith('--disable-warning'));
    if (!quietens) args.push('--disable-warning=UNDICI-EHPA');
    const child = spawn(process.execPath, [...args, ...process.argv.slice(1)], {
      stdio: 'inherit', // the serve protocol rides on stdin/stdout: the copy reads and writes them itself
      cwd: process.cwd(),
      env: { ...process.env, [ENV_PROXY_SWITCH]: '1', [LAUNCHER_PID]: String(process.pid) }
    });
    // a signal meant for "serve" arrives at this pid (the TUI and launchd kill the process they
    // started): pass it on. Ctrl-C from a terminal reaches the copy through the process group.
    for (const sig of ['SIGTERM', 'SIGHUP', 'SIGQUIT'] as const) {
      process.on(sig, () => {
        if (child.exitCode === null && child.signalCode === null) child.kill(sig);
      });
    }
    child.on('error', (e) => {
      console.error(`tango: could not restart with ${ENV_PROXY_SWITCH}: ${e.message}`);
      resolve(1);
    });
    child.on('exit', (code, signal) => {
      // leave the same trace the copy would have: its code, or 128 + the signal number
      resolve(code ?? (signal ? 128 + (osConstants.signals[signal] ?? 0) : 1));
    });
  });
}

/**
 * In a relaunched copy: exit when the launcher is gone (it was killed outright, so nothing will
 * ever pass on a signal), instead of living on as an orphan that holds the session.
 */
export function watchLauncher(env: NodeJS.ProcessEnv = process.env): void {
  const launcher = Number(env[LAUNCHER_PID]);
  if (!launcher) return;
  const timer = setInterval(() => {
    if (process.ppid !== launcher) process.exit(0);
  }, 2000);
  timer.unref(); // a check, not a reason to stay alive
}

/** One line for /doctor: how the proxy variables are being handled in this process. */
export function proxyStatus(env: NodeJS.ProcessEnv = process.env): string {
  const vars = PROXY_VARS.filter((k) => env[k]);
  const where = vars.length ? ` (${vars.map((k) => `${k}=${mask(env[k])}`).join(', ')})` : '';
  if (env[ENV_PROXY_SWITCH]) {
    return `${ENV_PROXY_SWITCH} on${env[LAUNCHER_PID] ? ', set by tango at start' : ''}${where}`;
  }
  const plan = proxyPlan(env);
  if (plan === 'off') return `off (no http(s)_proxy set)`;
  if (plan === 'runtime') return `http.setGlobalProxyFromEnv (Node ${process.versions.node})${where}`;
  if (plan === 'unsupported') return `ignored: Node ${process.versions.node} has no env-proxy support (22.21+ or 24+)${where}`;
  return `NODE_USE_ENV_PROXY needed${where}`;
}

/** A proxy URL may carry credentials: show the host, never user:password. */
function mask(url: string | undefined): string {
  try {
    const u = new URL(url!);
    return u.username || u.password ? `${u.protocol}//***@${u.host}` : `${u.protocol}//${u.host}`;
  } catch {
    return 'set';
  }
}
