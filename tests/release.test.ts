import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { mkdir, mkdtemp, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const REPO = process.cwd();
const sh = (cmd: string, args: string[], cwd = REPO) =>
  spawnSync(cmd, args, { cwd, encoding: 'utf8' });

test('the version is the same in package.json, tui/Cargo.toml and host.ts', () => {
  const pkg = JSON.parse(readFileSync(join(REPO, 'package.json'), 'utf8')).version;
  const cargo = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(join(REPO, 'tui/Cargo.toml'), 'utf8'))?.[1];
  const host = /export const VERSION = '([^']+)'/.exec(readFileSync(join(REPO, 'src/host.ts'), 'utf8'))?.[1];
  assert.equal(cargo, pkg, 'tui/Cargo.toml');
  assert.equal(host, pkg, 'src/host.ts');
});

test('install.sh and scripts/package.sh are valid POSIX sh', () => {
  for (const f of ['install.sh', 'scripts/package.sh']) {
    const r = sh('sh', ['-n', f]);
    assert.equal(r.status, 0, `${f}: ${r.stderr}`);
  }
  for (const f of ['install.sh', 'scripts/package.sh']) {
    assert.match(readFileSync(join(REPO, f), 'utf8'), /^#!\/bin\/sh\n/, `${f} runs with /bin/sh`);
  }
});

test('the install script reads a tag_name from the releases/latest JSON and tolerates a missing v', () => {
  // the sed that pulls the tag out of the API's JSON, taken from install.sh itself
  const line = readFileSync(join(REPO, 'install.sh'), 'utf8').split('\n').find((l) => l.includes("sed -n 's/.*\"tag_name\""))!;
  const sed = line.slice(line.indexOf('sed -n')).replace(/ \| head.*$/, '').trim();
  const extract = (json: string) => spawnSync('sh', ['-c', sed], { input: json, encoding: 'utf8' }).stdout.trim();
  assert.equal(extract('{"tag_name":"v0.2.0","name":"tango 0.2.0"}'), 'v0.2.0');
  assert.equal(extract('{\n  "tag_name": "v1.2.3",\n  "assets": []\n}'), 'v1.2.3', 'pretty-printed JSON too');
  assert.equal(extract('{"tag_name":"0.4.0"}'), 'v0.4.0', 'a tag without the v still yields one');
  assert.equal(extract('{"message":"Not Found"}'), '', 'nothing to extract');
});

test('scripts/package.sh lays out what the launcher expects', async (t) => {
  if (!existsSync(join(REPO, 'dist/cli.js')) || !existsSync(join(REPO, 'tui/target/release/tango-tui'))) {
    t.skip('needs `npm run build` and a release build of the TUI (the CI build jobs run it)');
    return;
  }
  const out = await mkdtemp(join(tmpdir(), 'tango-package-'));
  try {
    const r = sh('sh', ['scripts/package.sh', '9.9.9', 'darwin-arm64', out]);
    assert.equal(r.status, 0, r.stderr);
    const tarball = r.stdout.trim();
    assert.equal(tarball, join(out, 'tango-9.9.9-darwin-arm64.tar.gz'));
    assert.ok(existsSync(tarball));
    const unpack = join(out, 'unpack');
    await mkdir(unpack, { recursive: true });
    assert.equal(sh('tar', ['-xzf', tarball, '-C', unpack]).status, 0);
    const pkg = join(unpack, 'tango-9.9.9-darwin-arm64');
    for (const f of ['bin/tango.js', 'dist/cli.js', 'package.json', 'install.sh', 'install.ps1', 'VERSION', 'tui/target/release/tango-tui']) {
      assert.ok(existsSync(join(pkg, f)), `${f} is in the package`);
    }
    const version = readFileSync(join(pkg, 'VERSION'), 'utf8');
    assert.match(version, /^version 9\.9\.9$/m);
    assert.match(version, /^platform darwin-arm64$/m);
    assert.match(version, /^commit [0-9a-f]{7,}$/m);
    assert.deepEqual(
      (await readdir(join(pkg, 'dist'))).sort(),
      (await readdir(join(REPO, 'dist'))).sort(),
      'the whole dist goes in'
    );
  } finally {
    await rm(out, { recursive: true, force: true });
  }
});
