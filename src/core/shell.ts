/**
 * How tango runs a shell command on this platform.
 *
 * macOS and Linux: `bash -lc` — a login shell, so PATH carries the toolchains the user installed
 * (rustup, cargo, homebrew). Hooks keep the plain `sh -c` they always had.
 *
 * Windows: PowerShell — PowerShell 7 (`pwsh`) where it is installed, Windows PowerShell 5.1
 * everywhere else. The command is wrapped so that the console speaks UTF-8 (a Chinese Windows
 * console is GBK, and the model reads UTF-8) and so do `>`, Set-Content and Add-Content (5.1
 * writes UTF-16 otherwise, which nothing else can read); a failing cmdlet ends the command
 * with exit 1 instead of passing unnoticed, and a native command's exit code is the command's
 * own. 5.1 has no `&&`/`||`: outside quotes they become `; if ($?) { … }` / `; if (-not $?) { … }`.
 * A command that reads stdin gets it from a file instead: PowerShell reads a redirected stdin
 * itself and hands nothing to the native commands inside, so a hook's payload is written to a
 * temp file (hookStdin) and piped into the command by the script. TANGO_POWERSHELL names the
 * executable to use (a test can force 5.1 on a machine that has 7).
 *
 * Killing is per platform too: macOS and Linux signal the detached child's process group, so
 * grandchildren die with it; Windows has no process groups, and `taskkill /T /F` ends the tree.
 */
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { writeFileSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Writable } from 'node:stream';

export const IS_WINDOWS = process.platform === 'win32';

/**
 * Whether a shell child is spawned detached — a process group leader on macOS and Linux, so a
 * kill reaches its grandchildren. Windows has no process groups, and there a detached process
 * has no console: PowerShell, a console program, exits at once with status 0 and runs nothing.
 */
export const DETACHED = !IS_WINDOWS;

/** The PATH list separator: `:` on macOS and Linux, `;` on Windows. */
export const PATH_SEP = IS_WINDOWS ? ';' : ':';

export interface ShellInvocation {
  file: string;
  args: string[];
}

let psProbed: string | null | undefined; // undefined: not looked yet, null: no pwsh on this machine

/** The PowerShell to run: `pwsh` (7) where installed, `powershell.exe` (5.1) otherwise;
 *  TANGO_POWERSHELL names it outright. Probed once. */
export function powerShell(): string {
  const forced = process.env.TANGO_POWERSHELL?.trim();
  if (forced) return forced;
  if (psProbed === undefined) {
    const r = spawnSync('where.exe', ['pwsh.exe'], { stdio: 'ignore', timeout: 5000 });
    psProbed = r.status === 0 ? 'pwsh.exe' : null;
  }
  return psProbed ?? 'powershell.exe';
}

/**
 * Windows PowerShell 5.1 has no `&&` and `||`: outside quotes they become `; if ($?) { … }` and
 * `; if (-not $?) { … }`, the braces closed at the end, so `a && b && c` nests as it chains.
 */
export function translateChainOperators(command: string): string {
  let out = '';
  let quote: string | null = null;
  let depth = 0;
  for (let i = 0; i < command.length; i++) {
    const c = command[i];
    if (quote) {
      out += c;
      if (c === quote) quote = null; // PowerShell has no backslash escapes inside quotes (backtick is its own)
      continue;
    }
    if (c === '`') {
      out += c + (command[++i] ?? '');
      continue;
    }
    if (c === "'" || c === '"') {
      quote = c;
      out += c;
      continue;
    }
    if (c === '&' && command[i + 1] === '&') {
      out += '; if ($?) { ';
      depth++;
      i++;
      continue;
    }
    if (c === '|' && command[i + 1] === '|') {
      out += '; if (-not $?) { ';
      depth++;
      i++;
      continue;
    }
    out += c;
  }
  return depth ? out + ' }'.repeat(depth) : out;
}

/** What to spawn to run `command` as a shell command. `sh`: the POSIX hook shell, not a login
 *  bash. `stdin`: the command reads stdin (a hook's JSON payload). */
export function shellInvocation(command: string, opts: { sh?: boolean; stdin?: boolean } = {}): ShellInvocation {
  if (!IS_WINDOWS) {
    return opts.sh ? { file: 'sh', args: ['-c', command] } : { file: '/bin/bash', args: ['-lc', command] };
  }
  const ps = powerShell();
  const legacy = !/pwsh/i.test(ps);
  return {
    file: ps,
    args: [
      '-NoProfile',
      '-NonInteractive',
      '-ExecutionPolicy',
      'Bypass',
      '-Command',
      [
        // UTF-8 without a BOM everywhere: a console switched to [Text.Encoding]::UTF8 writes a BOM
        // ahead of the first output, which broke a hook's JSON; 5.1 writes UTF-16 through `>`,
        // Set-Content and Add-Content unless told otherwise. A process without a console (a hidden
        // one) cannot switch its code page at all: not fatal.
        '$utf8 = New-Object System.Text.UTF8Encoding($false)',
        'try { chcp.com 65001 > $null; [Console]::OutputEncoding = $utf8; $OutputEncoding = $utf8 } catch {}',
        '$PSDefaultParameterValues["Out-File:Encoding"] = $utf8',
        '$PSDefaultParameterValues["Set-Content:Encoding"] = $utf8',
        '$PSDefaultParameterValues["Add-Content:Encoding"] = $utf8',
        // commands people (and models) write on every system, as PowerShell takes them: `rm -f`
        // is ambiguous there (-Filter or -Force?), touch and which do not exist, mkdir has no -p
        [
          // an alias outranks a function in PowerShell: rm is Remove-Item's built-in alias, so it goes first
          'Remove-Item Alias:rm -ErrorAction SilentlyContinue',
          'function rm {',
          '  [CmdletBinding()] param(',
          '    [Alias("f", "force")] [switch] $Force, // parameter aliases are case-insensitive
          '    [Alias("r", "recurse", "recursive")] [switch] $Recurse,',
          '    [switch] $rf, [switch] $fr,',
          '    [Parameter(ValueFromRemainingArguments = $true)] [string[]] $Path)',
          '  Remove-Item -Path $Path -Force:($Force -or $rf -or $fr) -Recurse:($Recurse -or $rf -or $fr) -ErrorAction SilentlyContinue',
          '}',
          'function touch {',
          '  param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Path)',
          '  foreach ($p in $Path) {',
          '    if (Test-Path -LiteralPath $p) { (Get-Item -LiteralPath $p).LastWriteTime = Get-Date }',
          '    else { New-Item -ItemType File -Path $p | Out-Null }',
          '  }',
          '}',
          'function mkdir {',
          '  param([Alias("p")] [switch] $Parents, [Parameter(ValueFromRemainingArguments = $true)] [string[]] $Path)',
          '  foreach ($p in $Path) { New-Item -ItemType Directory -Path $p -Force:$Parents | Out-Null }',
          '}',
          'function which {',
          '  param([Parameter(Mandatory = $true)] [string] $Name)',
          '  (Get-Command $Name -ErrorAction SilentlyContinue).Source',
          '}',
        ].join('\n'),
        '$ErrorActionPreference = "Stop"',
        '$failed = $false',
        'try {',
        opts.stdin
          ? // the payload file the caller wrote (hookStdin) is piped into the command
            `if ($env:TANGO_HOOK_STDIN) { Get-Content -Raw -LiteralPath $env:TANGO_HOOK_STDIN | ${legacy ? translateChainOperators(command) : command} } else { ${legacy ? translateChainOperators(command) : command} }\n`
          : `${legacy ? translateChainOperators(command) : command}\n`,
        '} catch { [Console]::Error.WriteLine($_); $failed = $true }',
        'if ($failed) { exit 1 }',
        'if ($null -ne $LASTEXITCODE) { exit $LASTEXITCODE }',
        'exit 0'
      ].join('\n')
    ]
  };
}

/**
 * How a stdin-reading command (a hook) receives its payload. macOS and Linux: the caller writes
 * it to the child's stdin (`write`). Windows: PowerShell would read that stdin itself, so the
 * payload goes to a file named in `env` (TANGO_HOOK_STDIN, which the script pipes into the
 * command); the caller merges `env` into the child's environment and calls `cleanup` after.
 */
export function hookStdin(payload: string): { env: Record<string, string>; write?: (stdin: Writable) => void; cleanup: () => void } {
  if (!IS_WINDOWS) return { env: {}, write: (stdin) => stdin.end(payload), cleanup: () => {} };
  const file = join(tmpdir(), `tango-hook-stdin-${process.pid}-${randomBytes(6).toString('hex')}`);
  writeFileSync(file, payload, 'utf8');
  return { env: { TANGO_HOOK_STDIN: file }, cleanup: () => void unlink(file) };
}

function unlink(file: string): void {
  try {
    unlinkSync(file);
  } catch {}
}

/** The `TANGO_BIN` value the TUI spawns (program, then script), each double-quoted when it has
 *  a space: a Windows install lives under `C:\Program Files`, or in a user directory with a
 *  space in its name, and the TUI's split_bin honors the quotes. */
export function tangoBinValue(node: string, cli: string): string {
  const quote = (s: string) => (/\s/.test(s) ? `"${s}"` : s);
  return `${quote(node)} ${quote(cli)}`;
}

/**
 * End a shell command and everything it started. `pid` is a detached child's: on macOS and
 * Linux that is a process group leader, and the signal reaches the whole group; killing the
 * shell alone would orphan its children, which keep the output pipe open. On Windows the tree
 * under the pid is ended with taskkill.
 */
export function killProcessTree(pid: number | undefined, signal: NodeJS.Signals = 'SIGKILL'): void {
  if (!pid || pid <= 1) return;
  if (IS_WINDOWS) {
    spawnSync('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore', timeout: 10_000 });
    return;
  }
  try {
    process.kill(-pid, signal);
  } catch {
    try {
      process.kill(pid, signal);
    } catch {}
  }
}
