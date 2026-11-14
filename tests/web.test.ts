import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { decode, htmlToText, webFetchTool } from '../src/core/tools/web-fetch.js';
import { parseBing, pickBackend, unwrapBing, backends } from '../src/core/tools/web-search.js';

const ctx = { cwd: '.', signal: new AbortController().signal };
const out = (r: { content: any[] }) => r.content[0].text as string;

test('htmlToText keeps headings, links and lists; drops scripts and nav', () => {
  const html = `<html><head><title>T &amp; Co</title><script>evil()</script></head><body>
    <nav><a href="/x">menu</a></nav>
    <h1>Hello</h1><p>One <b>two</b>&nbsp;three</p>
    <ul><li>a</li><li><a href="/doc">docs</a></li></ul>
    <style>.x{}</style></body></html>`;
  const { title, text } = htmlToText(html, 'https://ex.com/page');
  assert.equal(title, 'T & Co');
  assert.match(text, /^# Hello/m);
  assert.match(text, /One two three/);
  assert.match(text, /- \[docs\]\(https:\/\/ex\.com\/doc\)/);
  assert.doesNotMatch(text, /evil|menu|\.x\{/);
});

test('decode honours GBK charset', () => {
  const gbk = Buffer.from([0xc4, 0xe3, 0xba, 0xc3]); // 你好
  assert.equal(decode(gbk, 'text/html; charset=gbk'), '你好');
  const withMeta = Buffer.concat([Buffer.from('<meta charset="gb2312">'), gbk]);
  assert.match(decode(withMeta, 'text/html'), /你好/);
});

test('web_fetch converts html, passes text through, rejects bad input', async () => {
  const server = createServer((req, res) => {
    if (req.url === '/page') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end('<html><head><title>Page</title></head><body><h2>Sub</h2><p>body text</p></body></html>');
    } else if (req.url === '/json') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end('{"ok":true}');
    } else if (req.url === '/img') {
      res.writeHead(200, { 'content-type': 'image/png' });
      res.end(Buffer.alloc(10));
    } else {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('nope');
    }
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  try {
    const page = await webFetchTool.execute({ url: `${base}/page` }, ctx);
    assert.match(out(page), /^200 .*\(text\/html\)\n# Page\n\n## Sub\n\nbody text$/);

    const json = await webFetchTool.execute({ url: `${base}/json` }, ctx);
    assert.match(out(json), /\{"ok":true\}/);

    const img = await webFetchTool.execute({ url: `${base}/img` }, ctx);
    assert.ok(img.isError);
    assert.match(out(img), /binary/);

    const missing = await webFetchTool.execute({ url: `${base}/gone` }, ctx);
    assert.ok(missing.isError);
    assert.match(out(missing), /^404/);

    assert.ok((await webFetchTool.execute({ url: 'file:///etc/passwd' }, ctx)).isError);
    assert.ok((await webFetchTool.execute({ url: 'not a url' }, ctx)).isError);
  } finally {
    server.close();
  }
});

test('parseBing extracts results and unwraps redirect links', () => {
  const target = 'https://nodejs.org/zh-cn';
  const href = `https://www.bing.com/ck/a?!&amp;&amp;p=abc&amp;u=a1${Buffer.from(target).toString('base64url')}&amp;ntb=1`;
  const html = `<ol><li class="b_algo"><h2><a href="${href}" h="x">Node.js &#8212; 运行 <strong>JS</strong></a></h2>
    <div><p class="b_lineclamp2">A runtime&nbsp;&#0183; fast</p></div></li>
    <li class="b_algo"><div>no link</div></li></ol>`;
  const r = parseBing(html);
  assert.equal(r.length, 1);
  assert.deepEqual(r[0], { url: target, title: 'Node.js — 运行 JS', snippet: 'A runtime · fast' });
  assert.equal(unwrapBing('https://example.com/a?b=1&amp;c=2'), 'https://example.com/a?b=1&c=2');
});

test('pickBackend: TANGO_SEARCH wins, then first configured key, then keyless bing', () => {
  const saved = { ...process.env };
  try {
    for (const b of backends) if (b.env) delete process.env[b.env];
    delete process.env.TANGO_SEARCH;
    assert.equal(pickBackend().name, 'bing');
    process.env.BRAVE_API_KEY = 'k';
    assert.equal(pickBackend().name, 'brave');
    process.env.TAVILY_API_KEY = 'k';
    assert.equal(pickBackend().name, 'tavily');
    process.env.TANGO_SEARCH = 'bing';
    assert.equal(pickBackend().name, 'bing');
    process.env.TANGO_SEARCH = 'nope';
    assert.throws(() => pickBackend(), /unknown backend/);
  } finally {
    process.env = saved;
  }
});
