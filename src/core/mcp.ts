/**
 * Minimal MCP (Model Context Protocol) stdio client: initialize → tools/list → tools/call.
 * Bridged tools register as mcp_<server>_<tool> with unknown write effects (treated as writes).
 * A dead or hanging server never takes serve down: requests time out, the server is marked
 * disconnected, and calls return an error text.
 */
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { VERSION } from '../host.js';

export interface McpServerSpec {
  command: string;
  args?: string[];
  env?: Record<string, string>;
}

export interface BridgedTool {
  server: string;
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
}

/** Bridged tool name: mcp_<server>_<tool>; appends _2/_3… when the name is already taken. */
export function bridgeName(server: string, tool: string, taken: Set<string>): string {
  const base = `mcp_${server}_${tool}`.replace(/[^a-zA-Z0-9_-]/g, '_');
  let name = base;
  let n = 2;
  while (taken.has(name)) name = `${base}_${n++}`;
  taken.add(name);
  return name;
}

export class McpServer {
  readonly name: string;
  connected = false;
  private child?: ChildProcessWithoutNullStreams;
  private seq = 0;
  private pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void; timer: NodeJS.Timeout }>();
  private lines = new EventEmitter();

  constructor(name: string, private spec: McpServerSpec) {
    this.name = name;
    this.lines.on('line', (line: string) => this.handleLine(line));
  }

  async start(): Promise<void> {
    this.stop(); // a restart replaces any half-dead child
    this.child = spawn(this.spec.command, this.spec.args ?? [], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: { ...process.env, ...(this.spec.env ?? {}) }
    });
    // spawn errors (bad command) arrive async — never let them kill serve (C1)
    this.child.on('error', (e) => {
      this.connected = false;
      for (const { reject, timer } of this.pending.values()) {
        clearTimeout(timer);
        reject(new Error(`mcp server "${this.name}" failed to start: ${e.message}`));
      }
      this.pending.clear();
    });
    // EPIPE on stdin after the server died is expected, not fatal
    this.child.stdin.on('error', () => {});
    let buf = '';
    this.child.stdout.setEncoding('utf8');
    this.child.stdout.on('data', (d: string) => {
      buf += d;
      let i: number;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (line) this.lines.emit('line', line);
      }
    });
    this.child.on('exit', () => {
      this.connected = false;
      for (const { reject, timer } of this.pending.values()) {
        clearTimeout(timer);
        reject(new Error(`mcp server "${this.name}" exited`));
      }
      this.pending.clear();
    });
    this.child.stderr?.on('data', (d: Buffer) => {
      const t = d.toString().trim();
      if (t) process.stderr.write(`[mcp ${this.name}] ${t}`.slice(0, 300) + '\n');
    });
    await this.request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: 'tango', version: VERSION }
    });
    this.notify('notifications/initialized', {});
    this.connected = true;
  }

  async listTools(): Promise<BridgedTool[]> {
    const r = await this.request('tools/list', {});
    return (r?.tools ?? []).map((t: any) => ({
      server: this.name,
      name: t.name,
      description: t.description ?? '',
      inputSchema: t.inputSchema ?? { type: 'object', properties: {} }
    }));
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<{ text: string; isError: boolean }> {
    // lazy reconnect: a dead server restarts on the next call (capped, lock-guarded so two
    // concurrent calls cannot kill each other's fresh child), instead of staying
    // disconnected for the whole session
    if (!this.connected || !this.child || this.child.exitCode !== null || this.child.killed) {
      if (this.reconnecting) {
        // another call is already bringing the server back; wait for its outcome
        await this.reconnecting.catch(() => {});
      } else if (this.reconnects < 3 || Date.now() - this.lastReconnectAt > 60_000) {
        this.reconnects++;
        this.lastReconnectAt = Date.now();
        this.reconnecting = this.start()
          .then(() => {
            this.reconnects = 0; // a successful handshake resets the budget
          })
          .finally(() => {
            this.reconnecting = undefined;
          });
        try {
          await this.reconnecting;
        } catch (e: any) {
          return { text: `mcp ${this.name}: reconnect failed (${e?.message ?? e})`, isError: true };
        }
      } else {
        return { text: `mcp ${this.name}: reconnect budget exhausted (3 attempts; retries again in a minute)`, isError: true };
      }
    }
    const r = await this.request('tools/call', { name, arguments: args });
    const text = (r?.content ?? [])
      .map((c: any) => (c.type === 'text' ? c.text : `[${c.type}]`))
      .join('\n');
    return { text, isError: r?.isError === true };
  }

  /** deterministic collision-free bridged name: mcp_<server>_<tool>, suffixed on collision */
  private reconnects = 0;
  private reconnecting: Promise<void> | undefined;
  private lastReconnectAt = 0;

  stop(): void {
    this.connected = false;
    try {
      this.child?.kill('SIGTERM');
    } catch {}
  }

  private handleLine(line: string): void {
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg.id !== undefined && this.pending.has(msg.id)) {
      const p = this.pending.get(msg.id)!;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.error) p.reject(new Error(`mcp ${this.name}: ${msg.error.message}`));
      else p.resolve(msg.result);
    }
  }

  private request(method: string, params: unknown): Promise<any> {
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      if (!this.child || this.child.killed) return reject(new Error(`mcp server "${this.name}" is not running`));
      const timer = setTimeout(() => {
        // a slow call fails, but does NOT permanently kill a live server (I2)
        this.pending.delete(id);
        reject(new Error(`mcp ${this.name}: ${method} timed out`));
      }, 10_000);
      this.pending.set(id, { resolve, reject, timer });
      if (this.child.exitCode !== null || this.child.killed) {
        clearTimeout(timer);
        this.pending.delete(id);
        return reject(new Error(`mcp server "${this.name}" is not running`));
      }
      this.child!.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }

  private notify(method: string, params: unknown): void {
    try {
      this.child?.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
    } catch {}
  }
}

/** Spawn+handshake every configured server; failures degrade to a disconnected entry. */
export async function connectServers(servers: Record<string, McpServerSpec> | undefined): Promise<McpServer[]> {
  const out: McpServer[] = [];
  for (const [name, spec] of Object.entries(servers ?? {})) {
    const s = new McpServer(name, spec);
    try {
      await s.start();
    } catch (e: any) {
      process.stderr.write(`[mcp ${name}] connect failed: ${e?.message ?? e}\n`);
      s.connected = false;
    }
    out.push(s);
  }
  return out;
}
