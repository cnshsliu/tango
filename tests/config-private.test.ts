import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, statSync, writeFileSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

async function withHome(run: (home: string, file: string) => Promise<void>) {
  const home = await mkdtemp(join(tmpdir(), 'tango-config-mode-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    await run(home, join(home, 'config.json'));
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
  }
}
const mode = (f: string) => statSync(f).mode & 0o777;

test('config.json is created readable by its owner only, and stays so when rewritten', async () => {
  await withHome(async (_home, file) => {
    const { saveConfig, loadConfig } = await import('../src/core/config.js');
    await saveConfig({ theme: 'dark' });
    assert.equal(mode(file), 0o600);
    await saveConfig({ theme: 'light', model: 'a/b' });
    assert.equal(mode(file), 0o600);
    assert.deepEqual(await loadConfig(), { theme: 'light', model: 'a/b' });
  });
});

test('config.json: a file that already exists keeps the mode its owner gave it', async () => {
  await withHome(async (_home, file) => {
    const { saveConfig } = await import('../src/core/config.js');
    writeFileSync(file, '{}\n', { mode: 0o644 });
    chmodSync(file, 0o644); // the umask may have narrowed it at creation
    await saveConfig({ theme: 'dark' });
    assert.equal(mode(file), 0o644, 'no chmod behind the owner\'s back');
  });
});
