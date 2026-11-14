import { spawn } from 'node:child_process';
import { readFile, stat, writeFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import type { Tool, ToolContext } from '../../types.js';
import { error, text, truncate } from '../registry.js';
import { docxToText, parseRange, pptxToText, xlsxToText, type Range } from './ooxml.js';
import { textToDocx } from './docx-write.js';
import { sheetsToXlsx, parseTsv, type Sheet } from './xlsx-write.js';
import { Zip } from './zip.js';

const MAX_FILE_BYTES = 200 * 1024 * 1024;
const TIMEOUT_MS = 60_000;

interface Input {
  path: string;
  pages?: string;
  sheet?: string;
  rows?: string;
}

function run(cmd: string, args: string[], ctx: ToolContext): Promise<{ out: string; err: string; code: number | null }> {
  return new Promise((done) => {
    const child = spawn(cmd, args, { cwd: ctx.cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    const out: Buffer[] = [];
    let err = '';
    child.stdout.on('data', (b: Buffer) => out.push(b));
    child.stderr.on('data', (b: Buffer) => (err += b.toString()));
    const kill = () => child.kill('SIGKILL');
    const timer = setTimeout(kill, TIMEOUT_MS);
    ctx.signal.addEventListener('abort', kill, { once: true });
    child.on('error', (e) => done({ out: '', err: e.message, code: -1 }));
    child.on('close', (code) => {
      clearTimeout(timer);
      ctx.signal.removeEventListener('abort', kill);
      done({ out: Buffer.concat(out).toString('utf8'), err, code });
    });
  });
}

// ---------- PDF ----------

/** JXA script: print each page's text separated by form feeds. argv: path, first, last. */
const PDFKIT_JXA = `
ObjC.import('PDFKit');
function run(argv) {
  const doc = $.PDFDocument.alloc.initWithURL($.NSURL.fileURLWithPath(argv[0]));
  if (!doc || doc.isNil()) throw new Error('cannot open PDF');
  const n = doc.pageCount;
  const first = Math.max(1, Number(argv[1])), last = Math.min(n, Number(argv[2]));
  const out = [];
  for (let i = first; i <= last; i++) {
    const s = doc.pageAtIndex(i - 1).string;
    out.push(s && !s.isNil() ? s.js : '');
  }
  return n + '\\n' + out.join('\\f');
}`;

async function pdfText(path: string, range: Range | undefined, ctx: ToolContext): Promise<string> {
  const first = range?.from ?? 1;
  const last = range?.to ?? Infinity;
  let pages: string[] | undefined;
  let total: number | undefined;
  let engine = 'pdftotext';

  const args = ['-layout', '-enc', 'UTF-8', '-f', String(first)];
  if (Number.isFinite(last)) args.push('-l', String(last));
  const r = await run('pdftotext', [...args, path, '-'], ctx);
  if (r.code === 0) {
    pages = r.out.split('\f');
    if (pages.length && !pages[pages.length - 1].trim()) pages.pop();
    const info = await run('pdfinfo', [path], ctx);
    const m = /^Pages:\s+(\d+)/m.exec(info.out);
    if (m) total = Number(m[1]);
  } else if (/password|encrypt/i.test(r.err)) {
    throw new Error('PDF is password-protected');
  } else if (process.platform === 'darwin') {
    const j = await run('osascript', ['-l', 'JavaScript', '-e', PDFKIT_JXA, path, String(first), String(Number.isFinite(last) ? last : 1e9)], ctx);
    if (j.code !== 0) throw new Error(`could not read PDF: ${(r.err || j.err).trim()}`);
    const nl = j.out.indexOf('\n');
    total = Number(j.out.slice(0, nl));
    pages = j.out.slice(nl + 1).replace(/\n$/, '').split('\f');
    engine = 'PDFKit (spacing may be off; `brew install poppler` for better output)';
  } else {
    throw new Error(`pdftotext not available (${r.err.trim() || 'not installed'}). Install poppler-utils.`);
  }

  const chars = pages.reduce((n, p) => n + p.replace(/\s/g, '').length, 0);
  const header = `(PDF, ${total ?? '?'} pages${range ? `, showing ${first}-${Math.min(last, total ?? last)}` : ''}; via ${engine})`;
  if (chars < 20 * Math.max(1, pages.length)) {
    return `${header}\nThis PDF has little or no text layer (${chars} characters over ${pages.length} pages). It is probably scanned images; OCR is needed to read it.`;
  }
  const body = pages.map((p, i) => `--- page ${first + i} ---\n${p.replace(/[ \t]+$/gm, '').replace(/\n{3,}/g, '\n\n').trim()}`);
  return [header, ...body].join('\n\n');
}

// ---------- tool ----------

export async function officeText(path: string, opts: Omit<Input, 'path'>, ctx: ToolContext): Promise<string> {
  const ext = extname(path).toLowerCase();
  const size = (await stat(path)).size;
  if (size > MAX_FILE_BYTES) throw new Error(`file is ${Math.round(size / 1e6)} MB, over the ${MAX_FILE_BYTES / 1e6} MB limit`);

  switch (ext) {
    case '.pdf':
      return pdfText(path, parseRange(opts.pages), ctx);
    case '.docx':
    case '.docm':
      return docxToText(new Zip(await readFile(path)));
    case '.xlsx':
    case '.xlsm':
      return xlsxToText(new Zip(await readFile(path)), { sheet: opts.sheet, rows: parseRange(opts.rows) });
    case '.pptx':
    case '.pptm':
      return pptxToText(new Zip(await readFile(path)), parseRange(opts.pages));
    case '.doc':
    case '.rtf':
    case '.odt':
    case '.wps': {
      if (process.platform !== 'darwin') throw new Error(`${ext} needs macOS textutil; convert to .docx first`);
      const r = await run('textutil', ['-convert', 'txt', '-stdout', path], ctx);
      if (r.code !== 0) throw new Error(`textutil failed: ${r.err.trim()}`);
      return r.out;
    }
    case '.xls':
    case '.ppt':
      throw new Error(`old binary ${ext} is not supported; save it as ${ext}x and retry`);
    default:
      throw new Error(`unsupported file type "${ext}". Supported: pdf, docx, xlsx, pptx, doc, rtf, odt`);
  }
}

export const officeReadTool: Tool<Input> = {
  name: 'office_read',
  description:
    'Extract text from PDF, Word (.docx/.doc/.rtf), Excel (.xlsx) and PowerPoint (.pptx) files. ' +
    'Word headings become #, tables become markdown. Excel returns tab-separated rows prefixed with the row number. ' +
    'PowerPoint returns each slide with its speaker notes. Use the read tool for plain text files.',
  input: {
    type: 'object',
    properties: {
      path: { type: 'string', description: 'File path, absolute or relative to cwd' },
      pages: { type: 'string', description: 'PDF pages or pptx slides, e.g. "3", "2-5", "10-"' },
      sheet: { type: 'string', description: 'xlsx: sheet name or 1-based index (default: all sheets)' },
      rows: { type: 'string', description: 'xlsx: row range, e.g. "1-100" (default: first 500)' }
    },
    required: ['path']
  },
  async execute({ path, ...opts }, ctx) {
    try {
      return text(truncate(await officeText(resolve(ctx.cwd, path), opts, ctx)));
    } catch (e: any) {
      return error(`office_read failed: ${e.message}`);
    }
  }
};

// ---------- write ----------

interface WriteInput {
  path: string;
  content: string | Sheet[];
}

function toSheets(content: string | Sheet[]): Sheet[] {
  if (Array.isArray(content)) {
    const sheets = content.map((s) => ({ name: s.name, rows: typeof s.rows === 'string' ? parseTsv(s.rows) : s.rows }));
    if (!sheets.length) throw new Error('sheets array is empty');
    return sheets;
  }
  const lines = content.split(/\r?\n/).filter((l) => l.trim());
  if (lines.length > 5 && !content.includes('\t'))
    throw new Error('xlsx content looks like plain prose, not tab-separated rows; pass one sheet per row of tab-separated cells (what office_read prints)');
  return [{ name: 'Sheet1', rows: parseTsv(content) }];
}

export const officeWriteTool: Tool<WriteInput> = {
  name: 'office_write',
  description:
    'Create a Word .docx or Excel .xlsx file. For .docx, content is markdown-ish text (# headings, - lists, | tables |, **bold**). ' +
    'For .xlsx, content is tab-separated rows (one sheet), or an array of {name, rows} objects for several sheets. ' +
    'Overwrites the file. Use the write tool for plain text.',
  input: {
    type: 'object',
    properties: {
      path: { type: 'string', description: 'Output path (.docx or .xlsx), absolute or relative to cwd' },
      content: {
        description: '.docx: markdown-ish text. .xlsx: tab-separated rows, or [{name, rows: tab-separated text}] per sheet.',
        oneOf: [{ type: 'string' }, { type: 'array', items: { type: 'object', properties: { name: { type: 'string' }, rows: { type: 'string' } }, required: ['rows'] } }]
      }
    },
    required: ['path', 'content']
  },
  async execute({ path, content }, ctx) {
    try {
      const file = resolve(ctx.cwd, path);
      const ext = extname(file).toLowerCase();
      let buf: Buffer;
      if (ext === '.docx') {
        buf = textToDocx(typeof content === 'string' ? content : content.map((s) => s.rows).join('\n'));
      } else if (ext === '.xlsx') {
        buf = sheetsToXlsx(toSheets(content));
      } else {
        return error(`office_write writes .docx and .xlsx only, got "${ext}"`);
      }
      await writeFile(file, buf);
      return text(`wrote ${buf.length} bytes → ${file}`);
    } catch (e: any) {
      return error(`office_write failed: ${e.message}`);
    }
  }
};
