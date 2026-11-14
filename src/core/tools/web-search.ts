import type { Tool } from '../types.js';
import { error, text, truncate } from './registry.js';

const UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36';

export interface SearchResult {
  title: string;
  url: string;
  snippet: string;
}

export interface SearchBackend {
  name: string;
  /** env var that enables it; undefined = always available */
  env?: string;
  search(query: string, count: number, signal: AbortSignal): Promise<SearchResult[]>;
}

async function postJson(url: string, headers: Record<string, string>, body: unknown, signal: AbortSignal) {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify(body),
    signal
  });
  if (!res.ok) throw new Error(`HTTP ${res.status} ${(await res.text()).slice(0, 300)}`);
  return res.json() as Promise<any>;
}

const NAMED: Record<string, string> = {
  nbsp: ' ', ensp: ' ', emsp: ' ', thinsp: ' ', lt: '<', gt: '>', quot: '"', apos: "'",
  mdash: '—', ndash: '–', hellip: '…', middot: '·', bull: '•', copy: '©', reg: '®', trade: '™',
  laquo: '«', raquo: '»', ldquo: '“', rdquo: '”', lsquo: '‘', rsquo: '’', times: '×', yen: '¥'
};

export function decodeEntities(s: string): string {
  return s
    .replace(/&([a-z]+);/gi, (m, n) => NAMED[n.toLowerCase()] ?? m)
    .replace(/&#x([0-9a-f]+);/gi, (_, h) => String.fromCodePoint(parseInt(h, 16)))
    .replace(/&#(\d+);/g, (_, d) => String.fromCodePoint(Number(d)))
    .replace(/&amp;/g, '&');
}

const stripTags = (s: string) => decodeEntities(s.replace(/<[^>]+>/g, '')).replace(/\s+/g, ' ').trim();

/** Bing wraps results in /ck/a?...&u=a1<base64url(target)>; unwrap to the real URL. */
export function unwrapBing(href: string): string {
  const raw = decodeEntities(href);
  try {
    const u = new URL(raw);
    const enc = u.searchParams.get('u');
    if (u.hostname.endsWith('bing.com') && enc?.startsWith('a1')) {
      return Buffer.from(enc.slice(2), 'base64url').toString('utf8');
    }
  } catch {}
  return raw;
}

export function parseBing(html: string): SearchResult[] {
  const out: SearchResult[] = [];
  for (const item of html.split('<li class="b_algo"').slice(1)) {
    const a = /<h2[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/.exec(item);
    if (!a) continue;
    const p = /<p[^>]*>([\s\S]*?)<\/p>/.exec(item);
    out.push({ url: unwrapBing(a[1]), title: stripTags(a[2]), snippet: stripTags(p?.[1] ?? '') });
  }
  return out;
}

export const backends: SearchBackend[] = [
  {
    name: 'tavily',
    env: 'TAVILY_API_KEY',
    async search(query, count, signal) {
      const j = await postJson(
        'https://api.tavily.com/search',
        { authorization: `Bearer ${process.env.TAVILY_API_KEY}` },
        { query, max_results: count },
        signal
      );
      return (j.results ?? []).map((r: any) => ({ title: r.title, url: r.url, snippet: r.content ?? '' }));
    }
  },
  {
    // 博查: China-mainland web search
    name: 'bocha',
    env: 'BOCHA_API_KEY',
    async search(query, count, signal) {
      const j = await postJson(
        'https://api.bochaai.com/v1/web-search',
        { authorization: `Bearer ${process.env.BOCHA_API_KEY}` },
        { query, count, summary: true },
        signal
      );
      return (j.data?.webPages?.value ?? []).map((r: any) => ({
        title: r.name,
        url: r.url,
        snippet: r.summary ?? r.snippet ?? ''
      }));
    }
  },
  {
    name: 'brave',
    env: 'BRAVE_API_KEY',
    async search(query, count, signal) {
      const res = await fetch(
        `https://api.search.brave.com/res/v1/web/search?q=${encodeURIComponent(query)}&count=${count}`,
        { headers: { accept: 'application/json', 'x-subscription-token': process.env.BRAVE_API_KEY ?? '' }, signal }
      );
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const j: any = await res.json();
      return (j.web?.results ?? []).map((r: any) => ({ title: r.title, url: r.url, snippet: stripTags(r.description ?? '') }));
    }
  },
  {
    // keyless fallback: scrapes the public results page, may break when Bing changes markup
    name: 'bing',
    async search(query, count, signal) {
      const res = await fetch(`https://www.bing.com/search?q=${encodeURIComponent(query)}&count=${count}`, {
        headers: { 'user-agent': UA, 'accept-language': 'zh-CN,zh;q=0.9,en;q=0.8' },
        signal
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return parseBing(await res.text());
    }
  }
];

/** TANGO_SEARCH picks a backend by name; otherwise the first one whose key is set. */
export function pickBackend(list = backends): SearchBackend {
  const wanted = process.env.TANGO_SEARCH;
  if (wanted) {
    const b = list.find((x) => x.name === wanted);
    if (!b) throw new Error(`TANGO_SEARCH=${wanted}: unknown backend (${list.map((x) => x.name).join(', ')})`);
    return b;
  }
  return list.find((b) => !b.env || process.env[b.env]) ?? list[list.length - 1];
}

export function formatResults(results: SearchResult[]): string {
  return results.map((r, i) => `[${i + 1}] ${r.title}\n${r.url}\n${r.snippet}`).join('\n\n');
}

export const webSearchTool: Tool<{ query: string; count?: number }> = {
  name: 'web_search',
  description:
    'Search the web. Returns numbered results with title, URL and snippet. Use web_fetch to read a result. ' +
    'Cite the URLs you rely on in your answer.',
  input: {
    type: 'object',
    properties: {
      query: { type: 'string' },
      count: { type: 'number', description: 'Results to return (default 8, max 20)' }
    },
    required: ['query']
  },
  async execute({ query, count = 8 }, ctx) {
    let backend: SearchBackend;
    try {
      backend = pickBackend();
    } catch (e: any) {
      return error(e.message);
    }
    try {
      const results = (await backend.search(query, Math.min(Math.max(count, 1), 20), ctx.signal)).slice(0, count);
      if (!results.length) return text(`no results (${backend.name})`);
      return text(truncate(`${formatResults(results)}\n\n(via ${backend.name})`));
    } catch (e: any) {
      return error(`web_search failed (${backend.name}): ${e.message}`);
    }
  }
};
