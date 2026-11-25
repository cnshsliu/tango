import { existsSync } from 'node:fs';
import { homedir } from 'node:os';

/**
 * A terminal whose directory was deleted under it (a removed worktree it still sits in) leaves
 * node without a working directory: `process.cwd()` throws from then on, and serve used to die
 * at startup with `ENOENT … uv_cwd` (the TUI: "tango serve exited"). Move to the home directory
 * once, before any subcommand asks.
 *
 * Returns the first half of the notice (what happened), or null when the launch directory is
 * fine; what is used instead depends on the session, so the caller finishes the sentence.
 */
export function rescueLaunchDir(): string | null {
  let gone = true;
  try {
    gone = !existsSync(process.cwd());
  } catch {}
  if (!gone) return null;
  const home = homedir();
  try {
    process.chdir(home);
  } catch {}
  return '⚠ tango 是在一个已不存在的目录里启动的（比如终端还停在已删除的 worktree 里）';
}
