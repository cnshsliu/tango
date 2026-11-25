import { test } from 'node:test';
import assert from 'node:assert/strict';
import { realpathSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { listCwds, rememberCwd } from '../src/core/cwds.ts';

test('remembered cwds are newest first and skip directories that are gone', async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-cwds-home-'));
  const a = await mkdtemp(join(tmpdir(), 'tango-cwds-a-'));
  const b = await mkdtemp(join(tmpdir(), 'tango-cwds-b-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    rememberCwd(a);
    rememberCwd(b);
    rememberCwd(join(home, 'not-there'));
    rememberCwd(a);
    const list = listCwds([join(home, 'also-missing'), b]);
    assert.deepEqual(list, [realpathSync(a), realpathSync(b)]);
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
    await rm(a, { recursive: true, force: true });
    await rm(b, { recursive: true, force: true });
  }
});
