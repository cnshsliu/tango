/** Tiny streaming XML tokenizer: good enough for OOXML parts, not a general parser. */
export type Token =
  | { kind: 'open' | 'self' | 'close'; name: string; attrs: string }
  | { kind: 'text'; text: string };

const TOKEN =
  /<!--[\s\S]*?-->|<!\[CDATA\[([\s\S]*?)\]\]>|<[!?][^>]*>|<(\/)?(?:[\w.-]+:)?([\w.-]+)((?:[^>"']|"[^"]*"|'[^']*')*?)(\/)?>|([^<]+)/g;

export function* tokens(xml: string): Generator<Token> {
  TOKEN.lastIndex = 0;
  for (let m: RegExpExecArray | null; (m = TOKEN.exec(xml)); ) {
    if (m[1] !== undefined) yield { kind: 'text', text: m[1] };
    else if (m[6] !== undefined) yield { kind: 'text', text: decodeXml(m[6]) };
    else if (m[3]) yield { kind: m[2] ? 'close' : m[5] ? 'self' : 'open', name: m[3], attrs: m[4] ?? '' };
  }
}

/** Read an attribute by local name, ignoring its namespace prefix. */
export function attr(attrs: string, name: string): string | undefined {
  const m = new RegExp(`(?:^|\\s)(?:[\\w.-]+:)?${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)')`).exec(attrs);
  return m ? decodeXml(m[1] ?? m[2]) : undefined;
}

/** The relationship id (r:id), which may sit next to a plain id attribute. */
export function relId(attrs: string): string | undefined {
  const m = /(?:^|\s)[\w.-]+:id\s*=\s*(?:"([^"]*)"|'([^']*)')/.exec(attrs);
  return m ? (m[1] ?? m[2]) : undefined;
}

const NAMED: Record<string, string> = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" };

export function decodeXml(s: string): string {
  if (!s.includes('&')) return s;
  return s.replace(/&(#x[0-9a-f]+|#\d+|\w+);/gi, (all, e: string) => {
    if (e[0] !== '#') return NAMED[e] ?? all;
    const cp = e[1] === 'x' || e[1] === 'X' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
    return Number.isFinite(cp) && cp <= 0x10ffff ? String.fromCodePoint(cp) : all;
  });
}

/** Parse a .rels part into id -> resolved zip path. */
export function rels(xml: string | undefined, baseDir: string): Map<string, { target: string; type: string }> {
  const out = new Map<string, { target: string; type: string }>();
  if (!xml) return out;
  for (const t of tokens(xml)) {
    if (t.kind === 'text' || t.name !== 'Relationship') continue;
    const id = attr(t.attrs, 'Id');
    const target = attr(t.attrs, 'Target');
    if (!id || !target || attr(t.attrs, 'TargetMode') === 'External') continue;
    out.set(id, { target: resolvePart(baseDir, target), type: attr(t.attrs, 'Type') ?? '' });
  }
  return out;
}

export function resolvePart(baseDir: string, target: string): string {
  const parts = target.startsWith('/') ? [] : baseDir.split('/').filter(Boolean);
  for (const seg of target.split('/')) {
    if (!seg || seg === '.') continue;
    if (seg === '..') parts.pop();
    else parts.push(seg);
  }
  return parts.join('/');
}
