import type { Zip } from './zip.js';
import { attr, relId, rels, tokens } from './xml.js';

export interface Range {
  from: number;
  to: number;
}

export function parseRange(s: string | undefined): Range | undefined {
  if (!s) return undefined;
  const m = /^\s*(\d+)\s*(?:-\s*(\d*)\s*)?$/.exec(s);
  if (!m) throw new Error(`bad range "${s}" (expected e.g. "3" or "3-10" or "3-")`);
  const from = Number(m[1]);
  const to = m[2] === undefined ? from : m[2] === '' ? Infinity : Number(m[2]);
  if (from < 1 || to < from) throw new Error(`bad range "${s}"`);
  return { from, to };
}

const inRange = (n: number, r?: Range) => !r || (n >= r.from && n <= r.to);

// ---------- docx ----------

function headingLevels(stylesXml: string | undefined): Map<string, number> {
  const out = new Map<string, number>();
  if (!stylesXml) return out;
  let id: string | undefined;
  for (const t of tokens(stylesXml)) {
    if (t.kind === 'text') continue;
    if (t.name === 'style' && t.kind === 'open') id = attr(t.attrs, 'styleId');
    else if (t.name === 'name' && id) {
      const name = (attr(t.attrs, 'val') ?? '').toLowerCase();
      const m = /^(?:heading|标题)\s*(\d)$/.exec(name);
      if (m) out.set(id, Number(m[1]));
      else if (name === 'title' || name === '标题') out.set(id, 1);
    }
  }
  return out;
}

export function docxToText(zip: Zip): string {
  const xml = zip.text('word/document.xml');
  if (!xml) throw new Error('not a .docx (word/document.xml missing)');
  const levels = headingLevels(zip.text('word/styles.xml'));
  const out: string[] = [];
  const tables: { rows: string[][]; row: string[]; cell: string[] }[] = [];
  let para = '';
  let level = 0;
  let listDepth = -1;
  let inText = false;
  let skip = 0; // inside mc:Fallback (duplicate of mc:Choice) or deleted text

  const endPara = () => {
    const s = para.replace(/[ \t]+$/g, '');
    para = '';
    const tbl = tables[tables.length - 1];
    if (tbl) {
      if (s.trim()) tbl.cell.push(s.trim());
    } else if (s.trim()) {
      const prefix = level ? '#'.repeat(level) + ' ' : listDepth >= 0 ? '  '.repeat(listDepth) + '- ' : '';
      out.push(prefix + s);
    }
    level = 0;
    listDepth = -1;
  };

  for (const t of tokens(xml)) {
    if (t.kind === 'text') {
      if (inText && !skip) para += t.text;
      continue;
    }
    const { name, kind } = t;
    if (name === 'Fallback' || name === 'del') {
      if (kind === 'open') skip++;
      else if (kind === 'close') skip = Math.max(0, skip - 1);
      continue;
    }
    if (skip) continue;
    if (name === 't') inText = kind === 'open';
    else if (name === 'p' && kind === 'close') endPara();
    else if (name === 'tab' && kind === 'self') para += '\t';
    else if ((name === 'br' || name === 'cr') && kind === 'self') para += '\n';
    else if (name === 'pStyle') level = levels.get(attr(t.attrs, 'val') ?? '') ?? 0;
    else if (name === 'ilvl') listDepth = Number(attr(t.attrs, 'val') ?? 0);
    else if (name === 'numPr' && kind !== 'close' && listDepth < 0) listDepth = 0;
    else if (name === 'tbl' && kind === 'open') tables.push({ rows: [], row: [], cell: [] });
    else if (name === 'tc' && kind === 'close') {
      const tbl = tables[tables.length - 1];
      if (tbl) tbl.row.push(tbl.cell.join(' / ').replace(/\|/g, '\\|').replace(/\n/g, ' ')), (tbl.cell = []);
    } else if (name === 'tr' && kind === 'close') {
      const tbl = tables[tables.length - 1];
      if (tbl) tbl.rows.push(tbl.row), (tbl.row = []);
    } else if (name === 'tbl' && kind === 'close') {
      const tbl = tables.pop();
      if (!tbl) continue;
      const md = tableToMarkdown(tbl.rows);
      const parent = tables[tables.length - 1];
      if (parent) parent.cell.push(tbl.rows.map((r) => r.join(' ; ')).join(' // '));
      else if (md) out.push(md);
    }
  }
  return out.join('\n\n');
}

function tableToMarkdown(rows: string[][]): string {
  if (!rows.length) return '';
  const width = Math.max(...rows.map((r) => r.length));
  const pad = (r: string[]) => '| ' + Array.from({ length: width }, (_, i) => r[i] ?? '').join(' | ') + ' |';
  return [pad(rows[0]), '|' + ' --- |'.repeat(width), ...rows.slice(1).map(pad)].join('\n');
}

// ---------- pptx ----------

/** Text of a DrawingML part: one line per paragraph, fields (slide numbers, dates) skipped. */
function drawingText(xml: string): string {
  const lines: string[] = [];
  let para = '';
  let inText = false;
  let skip = 0;
  for (const t of tokens(xml)) {
    if (t.kind === 'text') {
      if (inText && !skip) para += t.text;
      continue;
    }
    if (t.name === 'fld' || t.name === 'Fallback') {
      if (t.kind === 'open') skip++;
      else if (t.kind === 'close') skip = Math.max(0, skip - 1);
    } else if (t.name === 't') inText = t.kind === 'open';
    else if (t.name === 'br' && t.kind === 'self') para += '\n';
    else if (t.name === 'p' && t.kind === 'close') {
      if (para.trim()) lines.push(para.trim());
      para = '';
    }
  }
  return lines.join('\n');
}

export function pptxToText(zip: Zip, slides?: Range): string {
  const pres = zip.text('ppt/presentation.xml');
  if (!pres) throw new Error('not a .pptx (ppt/presentation.xml missing)');
  const presRels = rels(zip.text('ppt/_rels/presentation.xml.rels'), 'ppt');
  const paths: string[] = [];
  for (const t of tokens(pres)) {
    if (t.kind !== 'text' && t.name === 'sldId') {
      const target = presRels.get(relId(t.attrs) ?? '')?.target;
      if (target) paths.push(target);
    }
  }
  const out: string[] = [`(${paths.length} slides)`];
  paths.forEach((path, i) => {
    if (!inRange(i + 1, slides)) return;
    const dir = path.slice(0, path.lastIndexOf('/'));
    const file = path.slice(path.lastIndexOf('/') + 1);
    const xml = zip.text(path) ?? '';
    const body = drawingText(xml);
    const pics = (xml.match(/<p:pic>/g) ?? []).length;
    let section = `--- slide ${i + 1} ---\n${body || (pics ? `(no text; ${pics} image${pics > 1 ? 's' : ''})` : '(no text)')}`;
    for (const r of rels(zip.text(`${dir}/_rels/${file}.rels`), dir).values()) {
      if (!r.type.endsWith('/notesSlide')) continue;
      const notes = drawingText(zip.text(r.target) ?? '');
      if (notes) section += `\n[notes]\n${notes}`;
    }
    out.push(section);
  });
  return out.join('\n\n');
}

// ---------- xlsx ----------

const BUILTIN_DATE_FMTS = new Set([14, 15, 16, 17, 22, 27, 30, 36, 50, 57]);
const TIME_FMTS = new Set([18, 19, 20, 21, 45, 46, 47]);

/** Style index -> 'date' | 'time' | undefined, from cellXfs + numFmts. */
function dateStyles(stylesXml: string | undefined): ('date' | 'time' | undefined)[] {
  if (!stylesXml) return [];
  const custom = new Map<number, string>();
  const xfs: ('date' | 'time' | undefined)[] = [];
  let inCellXfs = false;
  for (const t of tokens(stylesXml)) {
    if (t.kind === 'text') continue;
    if (t.name === 'numFmt') custom.set(Number(attr(t.attrs, 'numFmtId')), attr(t.attrs, 'formatCode') ?? '');
    else if (t.name === 'cellXfs') inCellXfs = t.kind === 'open';
    else if (t.name === 'xf' && inCellXfs && t.kind !== 'close') {
      const id = Number(attr(t.attrs, 'numFmtId') ?? 0);
      const code = (custom.get(id) ?? '').replace(/"[^"]*"|\[[^\]]*\]|\\./g, '').toLowerCase();
      if (TIME_FMTS.has(id) || (!custom.has(id) ? false : /[hs]/.test(code) && !/[yd]/.test(code))) xfs.push('time');
      else if (BUILTIN_DATE_FMTS.has(id) || /[yd]/.test(code) || /m/.test(code)) xfs.push('date');
      else xfs.push(undefined);
    }
  }
  return xfs;
}

function serialToIso(v: number, kind: 'date' | 'time', date1904: boolean): string {
  const ms = Math.round((v + (date1904 ? 1462 : 0) - 25569) * 86400_000);
  const iso = new Date(ms).toISOString();
  if (kind === 'time' && v < 1) return iso.slice(11, 19);
  return iso.endsWith('T00:00:00.000Z') ? iso.slice(0, 10) : iso.slice(0, 19).replace('T', ' ');
}

const colIndex = (ref: string) => {
  let n = 0;
  for (const ch of ref.replace(/\d+$/, '')) n = n * 26 + (ch.toUpperCase().charCodeAt(0) - 64);
  return n - 1;
};

function sharedStrings(xml: string | undefined): string[] {
  const out: string[] = [];
  if (!xml) return out;
  let cur = '';
  let inText = false;
  let phonetic = 0;
  for (const t of tokens(xml)) {
    if (t.kind === 'text') {
      if (inText && !phonetic) cur += t.text;
      continue;
    }
    if (t.name === 'rPh') phonetic += t.kind === 'open' ? 1 : t.kind === 'close' ? -1 : 0;
    else if (t.name === 't') inText = t.kind === 'open';
    else if (t.name === 'si' && t.kind === 'close') out.push(cur), (cur = '');
    else if (t.name === 'si' && t.kind === 'self') out.push('');
  }
  return out;
}

export interface XlsxOptions {
  sheet?: string;
  rows?: Range;
}

const DEFAULT_ROWS = 500;

export function xlsxToText(zip: Zip, opts: XlsxOptions = {}): string {
  const wb = zip.text('xl/workbook.xml');
  if (!wb) throw new Error('not a .xlsx (xl/workbook.xml missing)');
  const wbRels = rels(zip.text('xl/_rels/workbook.xml.rels'), 'xl');
  const sheets: { name: string; path: string; hidden: boolean }[] = [];
  let date1904 = false;
  for (const t of tokens(wb)) {
    if (t.kind === 'text') continue;
    if (t.name === 'workbookPr') date1904 = /^(1|true)$/.test(attr(t.attrs, 'date1904') ?? '');
    if (t.name !== 'sheet') continue;
    const target = wbRels.get(relId(t.attrs) ?? '')?.target;
    if (target) sheets.push({ name: attr(t.attrs, 'name') ?? '?', path: target, hidden: !!attr(t.attrs, 'state')?.includes('hidden') });
  }
  const strings = sharedStrings(zip.text('xl/sharedStrings.xml'));
  const styles = dateStyles(zip.text('xl/styles.xml'));

  let chosen = sheets;
  if (opts.sheet) {
    const n = /^\d+$/.test(opts.sheet) ? Number(opts.sheet) : 0;
    chosen = sheets.filter((s, i) => s.name === opts.sheet || i + 1 === n);
    if (!chosen.length) throw new Error(`no sheet "${opts.sheet}". Sheets: ${sheets.map((s) => s.name).join(', ')}`);
  }

  const out: string[] = [`Sheets: ${sheets.map((s, i) => `${i + 1}. ${s.name}${s.hidden ? ' (hidden)' : ''}`).join(', ')}`];
  for (const sheet of chosen) {
    const range = opts.rows ?? { from: 1, to: DEFAULT_ROWS };
    const lines: string[] = [];
    let total = 0;
    let rowNum = 0;
    let row: string[] = [];
    let cell = { col: 0, type: '', style: 0, value: '' };
    let inValue = false;
    for (const t of tokens(zip.text(sheet.path) ?? '')) {
      if (t.kind === 'text') {
        if (inValue) cell.value += t.text;
        continue;
      }
      if (t.name === 'row' && t.kind !== 'close') {
        rowNum = Number(attr(t.attrs, 'r') ?? rowNum + 1);
        row = [];
      } else if (t.name === 'c' && t.kind !== 'close') {
        const ref = attr(t.attrs, 'r');
        cell = { col: ref ? colIndex(ref) : row.length, type: attr(t.attrs, 't') ?? 'n', style: Number(attr(t.attrs, 's') ?? 0), value: '' };
        if (t.kind === 'self') continue;
      } else if (t.name === 'v' || t.name === 't') inValue = t.kind === 'open';
      else if (t.name === 'c' && t.kind === 'close') {
        let v = cell.value;
        if (cell.type === 's') v = strings[Number(v)] ?? '';
        else if (cell.type === 'b') v = v === '1' ? 'TRUE' : 'FALSE';
        else if (cell.type === 'n' && v !== '' && styles[cell.style]) v = serialToIso(Number(v), styles[cell.style]!, date1904);
        else if (cell.type === 'n' && /[.eE]/.test(v) && Number.isFinite(Number(v))) v = String(Number(Number(v).toPrecision(15)));
        row[cell.col] = v.replace(/\r?\n/g, ' ').replace(/\t/g, ' ');
      } else if (t.name === 'row' && (t.kind === 'close' || t.kind === 'self')) {
        if (!row.some((v) => v)) continue;
        total++;
        if (inRange(rowNum, range)) lines.push(`${rowNum}\t${Array.from(row, (v) => v ?? '').join('\t')}`);
      }
    }
    const shown = lines.length;
    const note = total > shown ? ` (showing ${shown} of ${total} non-empty rows; use rows="a-b" for more)` : '';
    out.push(`## ${sheet.name}${note}\n${lines.join('\n') || '(empty)'}`);
  }
  return out.join('\n\n');
}
