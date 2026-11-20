/**
 * One-line plain-language summary of a shell command, for the permission
 * dialog: the user decides from the brief, the raw command stays available
 * below it (scrollable, and on the clipboard via `c`). Deterministic and
 * offline — this must never call a model or the network.
 */
export function briefCommand(cmd: string): string {
  const raw = cmd.trim();
  if (!raw) return '';
  // leading `cd X;` prefixes set the stage but rarely carry the effect
  let body = raw;
  let cd = '';
  while (true) {
    const m = /^cd\s+(\S+)\s*;\s*/.exec(body);
    if (!m) break;
    cd = m[1];
    body = body.slice(m[0].length);
  }
  const at = cd ? `（在 ${cd}）` : '';

  // heredoc: `cmd file <<'EOF' … EOF` — show the payload size and first line
  const heredoc = /^(.*?)<<['"]?(\w+)['"]?\n([\s\S]*?)\n?\2\s*$/.exec(body);
  if (heredoc) {
    const head = heredoc[1].trim();
    const payload = heredoc[3].split('\n').filter((l) => l.trim());
    const first = payload[0]?.trim().slice(0, 40) ?? '';
    const target = /(\S+)\s*$/.exec(head)?.[1] ?? '某文件';
    const verb = />>/.test(head) ? '追加' : '写入';
    return `向 ${target} ${verb} ${payload.length} 行文本（首行：${first}）${at}`;
  }

  // strip common prefix chains (env vars, sudo)
  const s = body.replace(/^(\w+=\S+\s+)+/, '').replace(/^sudo\s+/, '').trim();
  const words = s.split(/\s+/);
  const program = words[0] ?? '';
  const rest = words.slice(1).join(' ');

  const rmFlags = /^-[\w-]*r/.test(words[1] ?? '') && /^-[\w-]*f/.test(words[1] ?? '');
  const rmForceOnly = /^-[\w-]*f/.test(words[1] ?? '') && !rmFlags;

  switch (true) {
    case program === 'rm':
      return `删除${rmFlags ? '（递归+强制）' : rmForceOnly ? '（强制）' : ''}：${args(s)}${at}`;
    case program === 'mv':
      return `移动/重命名：${twoPaths(s)}${at}`;
    case program === 'cp':
    case program === 'rsync':
      return `复制：${twoPaths(s)}${at}`;
    case program === 'mkdir':
      return `创建目录：${args(s)}${at}`;
    case /(?:^|\s)>\s*(\S+)\s*$/.test(s) && !/>>/.test(s):
      return `覆盖写入 ${/(?:^|\s)>\s*(\S+)\s*$/.exec(s)?.[1] ?? ''}${at}`;
    case />>\s*(\S+)\s*$/.test(s):
      return `向 ${/>>\s*(\S+)\s*$/.exec(s)?.[1] ?? ''} 追加${at}`;
    case program === 'git': {
      const sub = words[1] ?? '';
      if (sub === 'commit')
        return `git 提交：${(/-m\s+["']([^"']+)["']/.exec(s)?.[1] ?? words.slice(2).join(' ')).slice(0, 50)}`;
      if (sub === 'push') return `git 推送 ${words.slice(2).join(' ')}`;
      if (sub === 'reset' && /--hard/.test(s)) return `git 重置（丢弃改动）到 ${words[words.length - 1]}`;
      if (sub === 'clean') return `git 清理未跟踪文件（${/-\w*x/.test(s) ? '含忽略文件' : '未跟踪'}）`;
      return `git ${sub} ${words.slice(2).join(' ')}`.trim();
    }
    case program === 'curl' || program === 'wget':
      return `下载 ${words.find((w) => /^https?:\/\//.test(w)) ?? ''}${at}`;
    case program === 'npm' || program === 'pnpm' || program === 'yarn':
      return /install|\bi\b/.test(rest)
        ? `${/-D|--save-dev/.test(rest) ? '安装开发依赖' : '安装'} npm 包：${words.filter((w) => !w.startsWith('-') && !['install', 'i', 'add'].includes(w) && w !== program).join(' ')}`
        : `${program} ${rest}`;
    case program === 'cargo' && words[1] === 'install':
      return `安装 cargo 包：${words.slice(2).join(' ')}`;
    case program === 'pip' || program === 'pip3':
      return /install/.test(rest)
        ? `安装 pip 包：${words.filter((w) => !w.startsWith('-') && w !== 'install' && w !== program).join(' ')}`
        : `${program} ${rest}`;
    default:
      return raw.replace(/\s+/g, ' ').slice(0, 60);
  }
}

/** arguments without the program and without flags */
function args(s: string): string {
  return s.split(/\s+/).slice(1).filter((w) => !w.startsWith('-')).join(' ');
}

/** "a → b" for two-path commands */
function twoPaths(s: string): string {
  const parts = s.split(/\s+/).slice(1).filter((w) => !w.startsWith('-'));
  if (parts.length >= 2) return `${parts[0]} → ${parts[parts.length - 1]}`;
  return parts.join(' ');
}
