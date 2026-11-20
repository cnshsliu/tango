import { test } from 'node:test';
import assert from 'node:assert/strict';

import { briefCommand } from '../src/core/bash-brief.js';

test('briefCommand summarizes common risky shapes in plain language', () => {
  // heredoc append: what file, how many lines, first line of the body
  const heredoc = "cd /Users/lucas/tango; cat >> docs/handbook.md <<'EOF'\n### Background work\nline2\nline3\nEOF";
  assert.equal(
    briefCommand(heredoc),
    '向 docs/handbook.md 追加 3 行文本（首行：### Background work）（在 /Users/lucas/tango）'
  );
  // rm with flags
  assert.equal(briefCommand('rm -rf /tmp/build /tmp/cache'), '删除（递归+强制）：/tmp/build /tmp/cache');
  assert.equal(briefCommand('rm a.txt'), '删除：a.txt');
  // overwrite / append redirects
  assert.equal(briefCommand('echo hi > out.txt'), '覆盖写入 out.txt');
  assert.equal(briefCommand('echo hi >> out.txt'), '向 out.txt 追加');
  // moves and copies
  assert.equal(briefCommand('mv a b'), '移动/重命名：a → b');
  assert.equal(briefCommand('cp a b'), '复制：a → b');
  // git
  assert.equal(briefCommand('git commit -m "fix thing"'), 'git 提交：fix thing');
  assert.equal(briefCommand('git push origin main'), 'git 推送 origin main');
  assert.equal(briefCommand('git reset --hard HEAD~1'), 'git 重置（丢弃改动）到 HEAD~1');
  // downloads / installs
  assert.equal(briefCommand('curl -fsSL https://x.dev/i.sh -o i.sh'), '下载 https://x.dev/i.sh');
  assert.equal(briefCommand('npm install left-pad'), '安装 npm 包：left-pad');
  // fallback keeps the head of the command
  assert.equal(briefCommand('some-obscure-tool --flag value'), 'some-obscure-tool --flag value');
});
