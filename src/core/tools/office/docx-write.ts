import { zipWrite } from './zip-write.js';

export const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

const XML_SAFE = /[\x00-\x08\x0b\x0c\x0e-\x1f]/g;
/** Control characters are illegal in OOXML and make Word refuse the file. */
export const sanitize = (s: string) => s.replace(XML_SAFE, '');

const W_NS =
  'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"';

function runs(text: string): string {
  // **bold** and `code` survive; other markdown inline markers are stripped
  const parts: string[] = [];
  const re = /\*\*([^*]+)\*\*|`([^`]+)`|\*([^*]+)\*/g;
  let last = 0;
  let m: RegExpExecArray | null;
  const push = (t: string, props = '') => {
    if (!t) return;
    parts.push(`<w:r>${props ? `<w:rPr>${props}</w:rPr>` : ''}<w:t xml:space="preserve">${esc(t)}</w:t></w:r>`);
  };
  while ((m = re.exec(text))) {
    push(text.slice(last, m.index));
    if (m[1]) push(m[1], '<w:b/>');
    else if (m[2]) push(m[2], '<w:rStyle w:val="Code"/>');
    else push(m[3], '<w:i/>');
    last = m.index + m[0].length;
  }
  push(text.slice(last));
  return parts.join('') || '<w:r><w:t xml:space="preserve"></w:t></w:r>';
}

function para(text: string, style?: string, listIndent?: number): string {
  const pr: string[] = [];
  if (style) pr.push(`<w:pStyle w:val="${style}"/>`);
  if (listIndent !== undefined) pr.push(`<w:numPr><w:ilvl w:val="${Math.min(listIndent, 8)}"/><w:numId w:val="1"/></w:numPr>`);
  return `<w:p>${pr.length ? `<w:pPr>${pr.join('')}</w:pPr>` : ''}${runs(text)}</w:p>`;
}

function table(rows: string[][]): string {
  const grid = rows[0].length;
  const trs = rows
    .map(
      (cells, i) =>
        `<w:tr>${Array.from({ length: grid }, (_, c) => cells[c] ?? '')
          .map((cell) => `<w:tc><w:tcPr><w:tcW w:w="0" w:type="auto"/></w:tcPr>${para(cell, i === 0 ? 'TableHeader' : undefined)}</w:tc>`)
          .join('')}</w:tr>`
    )
    .join('');
  return `<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/></w:tblPr>${trs}</w:tbl>`;
}

const splitRow = (line: string) =>
  line
    .replace(/^\|/, '')
    .replace(/\|\s*$/, '')
    .split(/(?<!\\)\|/)
    .map((c) => c.replace(/\\\|/g, '|').trim());

/** Markdown-ish text → a real .docx. Headings #..######, - / * / numbered lists, tables, **bold**, *italic*, `code`. */
export function textToDocx(text: string): Buffer {
  const body: string[] = [];
  const lines = sanitize(text).split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*$/.test(line)) continue;
    if (/^```/.test(line)) {
      const code: string[] = [];
      while (i + 1 < lines.length && !/^```/.test(lines[i + 1])) code.push(lines[++i]);
      i++;
      for (const c of code) body.push(para(c || ' ', 'Code'));
      continue;
    }
    const h = /^(#{1,6})\s+(.*)$/.exec(line);
    if (h) {
      body.push(para(h[2], `Heading${h[1].length}`));
      continue;
    }
    if (/^\s*\|/.test(line) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|?\s*$/.test(lines[i + 1])) {
      const rows = [splitRow(line)];
      i += 2;
      while (i < lines.length && /^\s*\|/.test(lines[i])) rows.push(splitRow(lines[i++]));
      body.push(table(rows));
      body.push('<w:p/>'); // a table cannot be the last block and needs spacing after
      continue;
    }
    const li = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/.exec(line);
    if (li) {
      body.push(para(li[3], 'ListParagraph', Math.floor(li[1].length / 2)));
      continue;
    }
    body.push(para(line.replace(/^(?:---+|\*\*\*+)$/, '⁘')));
  }
  if (!body.length) body.push('<w:p/>');

  const style = (id: string, name: string, based = 'Normal', extra = '') =>
    `<w:style w:type="paragraph" w:styleId="${id}"><w:name w:val="${name}"/>${based !== id ? `<w:basedOn w:val="${based}"/>` : ''}<w:pPr><w:keepNext/></w:pPr>${extra}</w:style>`;
  const styles = `<w:styles ${W_NS}>
    <w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:rFonts w:ascii="Calibri" w:eastAsia="微软雅黑"/><w:sz w:val="22"/></w:rPr></w:style>
    ${[1, 2, 3, 4, 5, 6].map((n) => style(`Heading${n}`, `heading ${n}`, 'Normal', `<w:rPr><w:b/><w:sz w:val="${36 - n * 3}"/></w:rPr>`)).join('')}
    ${style('ListParagraph', 'List Paragraph')}${style('TableHeader', 'Table Header', 'Normal', '<w:rPr><w:b/></w:rPr>')}
    <w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:rPr><w:rFonts w:ascii="Menlo" w:eastAsia="Menlo"/><w:sz w:val="20"/></w:rPr></w:style>
    <w:style w:type="character" w:styleId="Code"><w:name w:val="Code Char"/><w:rPr><w:rFonts w:ascii="Menlo"/><w:sz w:val="20"/></w:rPr></w:style>
    <w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="4" w:color="999999"/><w:left w:val="single" w:sz="4" w:color="999999"/><w:bottom w:val="single" w:sz="4" w:color="999999"/><w:right w:val="single" w:sz="4" w:color="999999"/><w:insideH w:val="single" w:sz="4" w:color="999999"/><w:insideV w:val="single" w:sz="4" w:color="999999"/></w:tblBorders></w:tblPr></w:style>
  </w:styles>`;

  return zipWrite({
    '[Content_Types].xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
        <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
        <Default Extension="xml" ContentType="application/xml"/>
        <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
        <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
        <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
      </Types>`,
    '_rels/.rels': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
      </Relationships>`,
    'word/_rels/document.xml.rels': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
        <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
      </Relationships>`,
    'word/document.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <w:document ${W_NS}><w:body>${body.join('')}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>`,
    'word/styles.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>${styles}`,
    'word/numbering.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
      <w:numbering ${W_NS}>
        <w:abstractNum w:abstractNumId="0">
          ${Array.from({ length: 9 }, (_, l) => `<w:lvl w:ilvl="${l}"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="${(l + 1) * 425}" w:hanging="213"/></w:pPr></w:lvl>`).join('')}
        </w:abstractNum>
        <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
      </w:numbering>`
  });
}
