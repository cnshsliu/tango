import { test as nodeTest } from 'node:test';
// Not on Windows: it reads `ps` output formats; the Windows path of the same code is covered by tests/shell.test.ts.
const test = process.platform === 'win32' ? nodeTest.skip : nodeTest;
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { isSameProcess, parseEtime, pidAlive, processStarts } from '../src/core/liveness.js';

const hasPs = spawnSync('ps', ['-o', 'pid=', '-p', String(process.pid)]).status === 0;
const iso = (ms: number) => new Date(ms).toISOString();

test('parseEtime reads every ps format', () => {
  assert.equal(parseEtime('05'), 5);
  assert.equal(parseEtime('1:03'), 63);
  assert.equal(parseEtime('21-04:51:59'), 21 * 86400 + 4 * 3600 + 51 * 60 + 59);
  assert.equal(parseEtime('0:00.13'), 0.13);
  assert.ok(Number.isNaN(parseEtime('garbage')));
});

test('processStarts: read from one ps call; a dead pid is left out', { skip: !hasPs }, () => {
  const gone = spawnSync(process.execPath, ['-e', '']).pid!;
  const starts = processStarts([process.pid, process.ppid, gone]);
  const now = Date.now(); // after the call: the estimate is taken inside it, and ps only knows whole seconds
  // this process started moments ago, its parent no later
  const mine = starts.get(process.pid)!;
  assert.ok(mine <= now + 1000 && mine >= now - 120_000, `start ${mine} now ${now}`);
  assert.ok(starts.get(process.ppid)! <= starts.get(process.pid)!, 'the parent is at least as old');
  assert.equal(starts.has(gone), false);
  assert.equal(pidAlive(0) || pidAlive(-1) || pidAlive(1), false, '0, -1 and 1 are never a holder');
});

test('isSameProcess: the writer must already have been running when the record was written', { skip: !hasPs }, () => {
  const started = processStarts([process.pid]).get(process.pid)!;
  // our own pid, recorded now: that is this process
  assert.equal(isSameProcess(process.pid, iso(Date.now()), started), true);
  // recorded 10 s before this process existed: the pid was somebody else's then
  assert.equal(isSameProcess(process.pid, iso(started - 10_000), started), false);
  // ps only knows whole seconds: a record written right at the start still counts
  assert.equal(isSameProcess(process.pid, iso(started + 1_500), started), true);
  // nothing to compare against: the plain "pid alive" answer
  assert.equal(isSameProcess(process.pid, undefined, started), true);
  assert.equal(isSameProcess(process.pid, 'not a date', started), true);
  assert.equal(isSameProcess(process.pid, iso(Date.now()), undefined), true);
  assert.equal(isSameProcess(99999999, iso(Date.now()), undefined), false);
});

test('session locks: a recycled pid is not a holder; a dead one never was', { skip: !hasPs }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-locks-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    const { sessionHolder, liveSessionLocks } = await import('../src/core/session-lock.js');
    const dir = join(home, 'locks', 'sessions');
    await mkdir(dir, { recursive: true });
    const write = (id: string, pid: number, at: string) => writeFile(join(dir, `${id}.json`), JSON.stringify({ pid, cwd: '/w', at }));
    const parent = process.ppid; // another live process, running since before this test
    const parentStarted = processStarts([parent]).get(parent)!;
    const gone = spawnSync(process.execPath, ['-e', '']).pid!;
    await write('live', parent, iso(Date.now()));
    await write('reused', parent, iso(parentStarted - 60_000)); // written a minute before that process existed
    await write('dead', gone, iso(Date.now()));
    assert.equal((await sessionHolder('live'))?.pid, parent);
    assert.equal(await sessionHolder('reused'), null);
    assert.equal(await sessionHolder('dead'), null);
    assert.deepEqual((await liveSessionLocks()).map((l) => l.sessionId), ['live']);
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
  }
});

test('cleanStaleLocks removes what nobody holds, in both lock directories', { skip: !hasPs }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-sweep-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    const { cleanStaleLocks } = await import('../src/core/session-lock.js');
    const root = join(home, 'locks');
    const sess = join(root, 'sessions');
    await mkdir(sess, { recursive: true });
    const parent = process.ppid;
    const parentStarted = processStarts([parent]).get(parent)!;
    const gone = spawnSync(process.execPath, ['-e', '']).pid!;
    const put = (file: string, body: unknown) => writeFile(file, typeof body === 'string' ? body : JSON.stringify(body));
    const now = iso(Date.now());
    // the top level: cwd locks
    await put(join(root, 'aaaa.json'), { pid: parent, at: now }); // live
    await put(join(root, 'bbbb.json'), { pid: gone, at: now }); // dead
    await put(join(root, 'cccc.json'), { pid: parent, at: iso(parentStarted - 60_000) }); // recycled pid
    await put(join(root, 'dddd.json'), '{not json'); // unreadable: left alone
    await put(join(root, 'notes.txt'), 'not a lock file'); // not even looked at
    // sessions/
    await put(join(sess, 's-live.json'), { pid: parent, at: now });
    await put(join(sess, 's-dead.json'), { pid: gone, at: now });
    assert.equal(await cleanStaleLocks(), 3);
    assert.deepEqual(readdirSync(root).sort(), ['aaaa.json', 'dddd.json', 'notes.txt', 'sessions']);
    assert.deepEqual(readdirSync(sess).sort(), ['s-live.json']);
    assert.equal(await cleanStaleLocks(), 0, 'nothing left to remove');
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
  }
});
