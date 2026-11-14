import { zipWrite } from './zip-write.js';
import { esc, sanitize } from './docx-write.js';

export interface Sheet {
  name: string;
  /** rows of cell values; numbers stay numbers, everything else becomes an inline string */
  rows: (string | number)[][];
}

const colName = (i: number) => {
  let s = '';
  for (i += 1; i > 0; i = Math.floor((i - 1) / 26)) s = String.fromCharCode(65 + ((i - 1) % 26)) + s;
  return s;
};

const num = (v: unknown) => typeof v === 'number' && Number.isFinite(v);

/** Parse content for one sheet: rows split on newlines, cells on tabs (what office_read prints). */
export function parseTsv(text: string): (string | number)[][] {
  return sanitize(text)
    .split(/\r?\n/)
    .map((line) => line.split('\t').map((c) => (c.trim() !== '' && Number.isFinite(Number(c)) ? Number(c) : c)));
}

function sheetXml(rows: (string | number)[][]): string {
  const body = rows
    .map(
      (cells, r) =>
        `<row r="${r + 1}">${cells
          .map((v, c) => {
            const ref = `${colName(c)}${r + 1}`;
            if (v === '' || v === null || v === undefined) return '';
            if (num(v)) return `<c r="${ref}"><v>${v}</v></c>`;
            return `<c r="${ref}" t="inlineStr"><is><t xml:space="preserve">${esc(String(v))}</t></is></c>`;
          })
          .join('')}</row>`
    )
    .join('');
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>${body}</sheetData></worksheet>`;
}

/** Build a minimal valid .xlsx. Sheet names are trimmed to Excel's 31-char limit and illegal chars. */
export function sheetsToXlsx(sheets: Sheet[]): Buffer {
  if (!sheets.length) throw new Error('no sheets to write');
  const used = new Set<string>();
  const clean = sheets.map((s, i) => {
    let name = sanitize(s.name || `Sheet${i + 1}`).replace(/[\\/*?:[\]]/g, ' ').trim().slice(0, 31) || `Sheet${i + 1}`;
    while (used.has(name.toLowerCase())) name = (name.slice(0, 28) + '~' + used.size).slice(0, 31);
    used.add(name.toLowerCase());
    return { ...s, name };
  });

  const files: Record<string, string> = {
    '[Content_Types].xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
        <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
        <Default Extension="xml" ContentType="application/xml"/>
        <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
        ${clean.map((_, i) => `<Override PartName="/xl/worksheets/sheet${i + 1}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>`).join('')}
      </Types>`,
    '_rels/.rels': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
      </Relationships>`,
    'xl/workbook.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
        <sheets>${clean.map((s, i) => `<sheet name="${esc(s.name)}" sheetId="${i + 1}" r:id="rId${i + 1}"/>`).join('')}</sheets>
      </workbook>`,
    'xl/_rels/workbook.xml.rels': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        ${clean.map((_, i) => `<Relationship Id="rId${i + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet${i + 1}.xml"/>`).join('')}
      </Relationships>`
  };
  clean.forEach((s, i) => (files[`xl/worksheets/sheet${i + 1}.xml`] = sheetXml(s.rows)));
  return zipWrite(files);
}
