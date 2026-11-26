import { test } from 'node:test';
import assert from 'node:assert/strict';
import { collectSecrets, parseEnvFile, redactText } from '../src/core/redact.js';

const K1 = 'sk-aaaaaaaaaaaaaaaaaaaaaaaa'; // 26
const K2 = 'sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'; // 42
const HOOK = 'https://open.feishu.cn/open-apis/bot/v2/hook/0123456789abcdef';

test('parseEnvFile: KEY=VALUE, quotes, export, comments, blank lines', () => {
  assert.deepEqual(parseEnvFile(`# a comment\n\nTKNET_API_KEY=${K1}\nexport QUOTED="${K2}"\nSINGLE='${K1}'\nnot a pair\n`), [
    ['TKNET_API_KEY', K1],
    ['QUOTED', K2],
    ['SINGLE', K1]
  ]);
  assert.deepEqual(parseEnvFile(''), []);
});

test('collectSecrets: every source, the name it is called by, and what is left out', () => {
  const pat = 'github_pat_' + 'x'.repeat(30);
  const secrets = collectSecrets({
    envFile: `# comment\nTKNET_API_KEY=${K1}\nexport SHORT=abc\nPATH_LIKE=/usr/local/bin/something\nQUOTED="${K2}"\n`,
    config: {
      feishuWebhook: HOOK,
      mcpServers: { sentry: { headers: { Authorization: `Bearer ${K2}` }, env: { SENTRY_TOKEN: pat } } }
    },
    models: [{ id: 'x/y', apiKey: 'SOME_ENV_NAME' }, { id: 'literal/one', apiKey: K1 }, { id: 'tiny', apiKey: 'sk-test' }],
    env: {
      GITHUB_PAT: pat,
      SSH_KEY_PATH: '/home/me/.ssh/id_ed25519',
      SSH_AUTH_SOCK: '/private/tmp/com.apple.launchd-xyz/Listeners',
      PLAIN_DESCRIPTION: 'nothing secret about this value at all'
    }
  });
  const names = (value: string) => secrets.filter((s) => s.value === value).map((s) => s.name);
  assert.deepEqual(names(K1), ['TKNET_API_KEY'], 'the env file comes first; the model entry with the same key adds no second name');
  assert.deepEqual(names(K2), ['QUOTED'], 'Bearer <key> counts the key alone');
  assert.deepEqual(names(HOOK), ['FEISHU_WEBHOOK']);
  assert.deepEqual(names(pat), ['mcp sentry SENTRY_TOKEN'], 'the config comes before the environment');
  // left out: short values, file paths (even under a secret-looking name), values under plain names, env-var names
  const values = secrets.map((s) => s.value);
  for (const not of ['abc', 'sk-test', '/usr/local/bin/something', '/home/me/.ssh/id_ed25519', 'SOME_ENV_NAME', 'nothing secret about this value at all', 'Bearer']) {
    assert.ok(!values.includes(not), not);
  }
  // longest first: when one secret contains another, the longer one is replaced first
  for (let i = 1; i < secrets.length; i++) assert.ok(secrets[i - 1].value.length >= secrets[i].value.length);
});

test('redactText: whole values only, also as a JSON string, other text untouched', () => {
  const secrets = [
    { name: 'A_KEY', value: 'sk-abcdef0123456789' },
    { name: 'HOOK', value: 'https://x.example/hook/"quoted"' }
  ];
  assert.equal(redactText(`key=sk-abcdef0123456789 ok`, secrets), 'key=[REDACTED A_KEY] ok');
  assert.equal(redactText('nothing to see', secrets), 'nothing to see');
  assert.equal(redactText('sk-abcdef0123', secrets), 'sk-abcdef0123', 'a prefix alone is not the secret');
  // inside a JSON document the quotes are escaped
  assert.equal(redactText('{"url":"https://x.example/hook/\\"quoted\\""}', secrets), '{"url":"[REDACTED HOOK]"}');
  // two occurrences, and a longer secret that contains the shorter one
  const nested = [
    { name: 'LONG', value: 'sk-abcdef0123456789-with-a-tail' },
    { name: 'SHORT', value: 'sk-abcdef0123456789' }
  ];
  assert.equal(redactText('a: sk-abcdef0123456789-with-a-tail b: sk-abcdef0123456789 c', nested), 'a: [REDACTED LONG] b: [REDACTED SHORT] c');
});
