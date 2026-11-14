import type { Tool } from '../types.js';
import { error, text, truncate } from './registry.js';
import { decodeEntities } from './web-search.js';

const UA =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36';
const MAX_BYTES = 5_000_000;
const TIMEOUT_MS = 30_000;

/** Turn an HTML page into markdown-ish text: headings, links, list items and paragraphs survive. */
export function htmlToText(html: string, base?: string): { title: string; text: string } {
  const title = decodeEntities(/<title[^>]*>([\s\S]*?)<\/title>/i.exec(html)?.[1] ?? '')
    .replace(/\s+/g, ' ')
    .trim();
  html = html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<(script|style|noscript|svg|template|iframe|textarea)\b[\s\S]*?<\/\1>/gi, '');
  let body = /<body[^>]*>([\s\S]*)<\/body>/i.exec(html)?.[1] ?? html;
  // prefer the main content when the page marks it
  const main = /<(main|article)[^>]*>([\s\S]*?)<\/\1>/i.exec(body);
  if (main && main[2].length > 500) body = main[2];

  const abs = (href: string) => {
    try {
      return new URL(decodeEntities(href), base).href;
    } catch {
      return href;
    }
  };

  let s = body
    .replace(/<(head|nav|footer|form)\b[\s\S]*?<\/\1>/gi, '')
    .replace(/<(h[1-6])[^>]*>([\s\S]*?)<\/\1>/gi, (_, h, t) => `\n\n${'#'.repeat(Number(h[1]))} ${t}\n\n`)
    .replace(/<a\b[^>]*href="([^"#][^"]*)"[^>]*>([\s\S]*?)<\/a>/gi, (_, href, t) => {
      const label = t.replace(/<[^>]+>/g, '').trim();
      return label ? `[${label}](${abs(href)})` : '';
    })
    .replace(/<li[^>]*>/gi, '\n- ')
    .replace(/<(pre)[^>]*>([\s\S]*?)<\/pre>/gi, (_, __, t) => `\n\n\`\`\`\n${t}\n\`\`\`\n\n`)
    .replace(/<br\s*\/?>/gi, '\n')
    .replace(/<\/(p|div|section|tr|table|ul|ol|blockquote|header)>/gi, '\n\n')
    .replace(/<(td|th)[^>]*>/gi, ' | ')
    .replace(/<[^>]+>/g, '');
  s = decodeEntities(s)
    .replace(/[ \t\f\v\r]+/g, ' ')
    .replace(/ *\n */g, '\n')
    .replace(/^- *\n+(?=\S)/gm, '- ')
    .replace(/^- *$\n?/gm, '')
    .replace(/\n{3,}/g, '\n\n')
    .trim();
  return { title, text: s };
}

export const webFetchTool: Tool<{ url: string; raw?: boolean }> = {
  name: 'web_fetch',
  description:
    'Fetch a web page or file over http(s) and return it as readable text (HTML is converted to markdown-like text). ' +
    'Set raw=true to get the unconverted body.',
  input: {
    type: 'object',
    properties: {
      url: { type: 'string', description: 'http:// or https:// URL' },
      raw: { type: 'boolean', description: 'Return the body unconverted (default false)' }
    },
    required: ['url']
  },
  async execute({ url, raw }, ctx) {
    let u: URL;
    try {
      u = new URL(url);
    } catch {
      return error(`invalid URL: ${url}`);
    }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return error('only http and https URLs are supported');

    const signal = AbortSignal.any([ctx.signal, AbortSignal.timeout(TIMEOUT_MS)]);
    let res: Response;
    try {
      res = await fetch(u, {
        headers: { 'user-agent': UA, accept: 'text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.5' },
        redirect: 'follow',
        signal
      });
    } catch (e: any) {
      return error(`web_fetch failed: ${e.name === 'TimeoutError' ? `timed out after ${TIMEOUT_MS / 1000}s` : e.message}`);
    }

    const type = res.headers.get('content-type') ?? '';
    const head = `${res.status} ${res.url}${type ? ` (${type.split(';')[0]})` : ''}`;
    if (/^(image|audio|video)\/|application\/(pdf|zip|octet-stream)/.test(type)) {
      await res.body?.cancel();
      return error(`${head}\nbinary content; web_fetch only returns text`);
    }

    const body = await readCapped(res);
    if (!res.ok) return error(`${head}\n${truncate(body, 2000)}`);
    if (raw || !/html|xml/.test(type)) return text(truncate(`${head}\n\n${body}`));
    const page = htmlToText(body, res.url);
    return text(truncate(`${head}\n${page.title ? `# ${page.title}\n` : ''}\n${page.text}`));
  }
};

async function readCapped(res: Response): Promise<string> {
  if (!res.body) return '';
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  while (size < MAX_BYTES) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    size += value.byteLength;
  }
  if (size >= MAX_BYTES) await reader.cancel();
  const bytes = Buffer.concat(chunks);
  return decode(bytes, res.headers.get('content-type') ?? '');
}

/** Honour the header charset, else a <meta charset> in the first 2KB; GBK pages are common in China. */
export function decode(bytes: Uint8Array, contentType: string): string {
  const sniff = Buffer.from(bytes.subarray(0, 2048)).toString('latin1');
  const charset = (
    /charset=["']?([\w-]+)/i.exec(contentType)?.[1] ??
    /<meta[^>]+charset=["']?([\w-]+)/i.exec(sniff)?.[1] ??
    'utf-8'
  ).toLowerCase();
  try {
    return new TextDecoder(charset === 'gb2312' ? 'gbk' : charset).decode(bytes);
  } catch {
    return new TextDecoder().decode(bytes);
  }
}
