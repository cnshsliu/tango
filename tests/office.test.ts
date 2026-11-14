import { test } from 'node:test';
import assert from 'node:assert/strict';
import { deflateRawSync } from 'node:zlib';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Zip } from '../src/core/tools/office/zip.js';
import { decodeXml, resolvePart } from '../src/core/tools/office/xml.js';
import { parseRange } from '../src/core/tools/office/ooxml.js';
import { officeReadTool, officeWriteTool } from '../src/core/tools/office/index.js';

const ctx = { cwd: mkdtempSync(join(tmpdir(), 'tango-office-')), signal: new AbortController().signal };
const out = (r: { content: any[] }) => r.content[0].text as string;

/** Build a zip in memory (CRC left 0; the reader does not check it). */
function zip(files: Record<string, string>, store = false): Buffer {
  const locals: Buffer[] = [];
  const central: Buffer[] = [];
  let offset = 0;
  for (const [name, body] of Object.entries(files)) {
    const raw = Buffer.from(body, 'utf8');
    const data = store ? raw : deflateRawSync(raw);
    const n = Buffer.from(name);
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0);
    lh.writeUInt16LE(20, 4);
    lh.writeUInt16LE(store ? 0 : 8, 8);
    lh.writeUInt32LE(data.length, 18);
    lh.writeUInt32LE(raw.length, 22);
    lh.writeUInt16LE(n.length, 26);
    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0);
    ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(store ? 0 : 8, 10);
    ch.writeUInt32LE(data.length, 20);
    ch.writeUInt32LE(raw.length, 24);
    ch.writeUInt16LE(n.length, 28);
    ch.writeUInt32LE(offset, 42);
    locals.push(lh, n, data);
    central.push(ch, n);
    offset += 30 + n.length + data.length;
  }
  const cd = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(Object.keys(files).length, 8);
  end.writeUInt16LE(Object.keys(files).length, 10);
  end.writeUInt32LE(cd.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, end]);
}

function fixture(name: string, files: Record<string, string>): string {
  writeFileSync(join(ctx.cwd, name), zip(files));
  return name;
}

const W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"';
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';

test('zip reader handles stored and deflated entries', () => {
  const z = new Zip(zip({ 'a.txt': 'hello', 'dir\\b.txt': '中文' }, true));
  assert.equal(z.text('a.txt'), 'hello');
  assert.equal(z.text('dir/b.txt'), '中文');
  assert.equal(new Zip(zip({ 'x.xml': 'x'.repeat(10_000) })).text('x.xml')!.length, 10_000);
  assert.throws(() => new Zip(Buffer.from('not a zip at all, definitely not')), /not a zip/);
});

test('xml helpers', () => {
  assert.equal(decodeXml('a &amp; b &lt;c&gt; &#20013;&#x6587; &bogus;'), 'a & b <c> 中文 &bogus;');
  assert.equal(resolvePart('ppt/slides', '../notesSlides/n1.xml'), 'ppt/notesSlides/n1.xml');
  assert.equal(resolvePart('xl', '/xl/worksheets/sheet1.xml'), 'xl/worksheets/sheet1.xml');
  assert.deepEqual(parseRange('3'), { from: 3, to: 3 });
  assert.deepEqual(parseRange('2-5'), { from: 2, to: 5 });
  assert.equal(parseRange('4-')!.to, Infinity);
  assert.throws(() => parseRange('5-2'));
});

test('docx: headings, lists, tables, breaks; skips deleted text and fallbacks', async () => {
  const p = fixture('a.docx', {
    'word/styles.xml': `<w:styles ${W}><w:style w:styleId="H1"><w:name w:val="heading 1"/></w:style>
      <w:style w:styleId="T"><w:name w:val="Title"/></w:style></w:styles>`,
    'word/document.xml': `<w:document ${W}><w:body>
      <w:p><w:pPr><w:pStyle w:val="H1"/></w:pPr><w:r><w:t>概览</w:t></w:r></w:p>
      <w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world &amp; co</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>
      <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>nested item</w:t></w:r></w:p>
      <w:p><w:r><w:t>line1</w:t><w:br/><w:t>line2</w:t></w:r></w:p>
      <mc:AlternateContent><mc:Choice><w:p><w:r><w:t>choice</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent>
      <w:tbl><w:tr><w:tc><w:p><w:r><w:t>名称</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>数量</w:t></w:r></w:p></w:tc></w:tr>
        <w:tr><w:tc><w:p><w:r><w:t>a|b</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
    </w:body></w:document>`
  });
  const r = await officeReadTool.execute({ path: p }, ctx);
  const s = out(r);
  assert.ok(!r.isError, s);
  assert.match(s, /^# 概览$/m);
  assert.match(s, /^Hello world & co$/m);
  assert.match(s, /^ {2}- nested item$/m);
  assert.match(s, /line1\nline2/);
  assert.match(s, /choice/);
  assert.doesNotMatch(s, /gone|fallback/);
  assert.match(s, /\| 名称 \| 数量 \|\n\| --- \| --- \|\n\| a\\\|b \| 2 \|/);
});

test('xlsx: shared strings, numbers, dates, booleans, sheet and row selection', async () => {
  const p = fixture('b.xlsx', {
    'xl/workbook.xml': `<workbook xmlns:r="${R}"><sheets>
      <sheet name="销售" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2" state="hidden"/></sheets></workbook>`,
    'xl/_rels/workbook.xml.rels': `<Relationships><Relationship Id="rId1" Type="x/worksheet" Target="worksheets/sheet1.xml"/>
      <Relationship Id="rId2" Type="x/worksheet" Target="/xl/worksheets/sheet2.xml"/></Relationships>`,
    'xl/sharedStrings.xml': `<sst><si><t>城市</t></si><si><r><t>金</t></r><r><t>额</t></r></si><si><t>北京</t></si><si><t>日期</t></si></sst>`,
    'xl/styles.xml': `<styleSheet><numFmts><numFmt numFmtId="164" formatCode="yyyy&quot;年&quot;m&quot;月&quot;"/></numFmts>
      <cellXfs><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/><xf numFmtId="10"/></cellXfs></styleSheet>`,
    'xl/worksheets/sheet1.xml': `<worksheet><sheetData>
      <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>3</v></c></row>
      <row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>1234.5</v></c><c r="C2" s="1"><v>45658</v></c><c r="E2" t="b"><v>1</v></c></row>
      <row r="3"><c r="A3" t="inlineStr"><is><t>上海</t></is></c><c r="B3" s="3"><v>0.25</v></c><c r="C3" s="2"><v>45689</v></c></row>
      <row r="4"/>
    </sheetData></worksheet>`,
    'xl/worksheets/sheet2.xml': `<worksheet><sheetData><row r="1"><c r="A1" t="str"><f>1+1</f><v>2</v></c></row></sheetData></worksheet>`
  });
  const all = out(await officeReadTool.execute({ path: p }, ctx));
  assert.match(all, /Sheets: 1\. 销售, 2\. Other \(hidden\)/);
  assert.match(all, /^1\t城市\t金额\t日期$/m);
  assert.match(all, /^2\t北京\t1234\.5\t2025-01-01\t\tTRUE$/m);
  assert.match(all, /^3\t上海\t0\.25\t2025-02-01$/m);
  assert.match(all, /## Other\n1\t2/);

  const one = out(await officeReadTool.execute({ path: p, sheet: '销售', rows: '2-2' }, ctx));
  assert.doesNotMatch(one, /## Other|^1\t/m);
  assert.match(one, /showing 1 of 3 non-empty rows/);
  assert.match(out(await officeReadTool.execute({ path: p, sheet: '2' }, ctx)), /## Other/);
  const bad = await officeReadTool.execute({ path: p, sheet: 'nope' }, ctx);
  assert.ok(bad.isError);
  assert.match(out(bad), /Sheets: 销售, Other/);
});

test('pptx: slides in presentation order with notes; fields skipped; slide range', async () => {
  const p = fixture('c.pptx', {
    'ppt/presentation.xml': `<p:presentation xmlns:r="${R}"><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>`,
    'ppt/_rels/presentation.xml.rels': `<Relationships><Relationship Id="rId2" Type="x/slide" Target="slides/slide1.xml"/>
      <Relationship Id="rId3" Type="x/slide" Target="slides/slide2.xml"/></Relationships>`,
    'ppt/slides/slide2.xml': `<p:sld><a:p><a:r><a:t>第一页标题</a:t></a:r></a:p><a:p><a:r><a:t>要点</a:t></a:r><a:fld type="slidenum"><a:t>1</a:t></a:fld></a:p></p:sld>`,
    'ppt/slides/_rels/slide2.xml.rels': `<Relationships><Relationship Id="rId1" Type="http://x/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>`,
    'ppt/notesSlides/notesSlide1.xml': `<p:notes><a:p><a:r><a:t>记得讲数据</a:t></a:r></a:p></p:notes>`,
    'ppt/slides/slide1.xml': `<p:sld><a:p><a:r><a:t>Second</a:t></a:r></a:p></p:sld>`
  });
  const s = out(await officeReadTool.execute({ path: p }, ctx));
  assert.match(s, /\(2 slides\)/);
  assert.match(s, /--- slide 1 ---\n第一页标题\n要点\n\[notes\]\n记得讲数据/);
  assert.match(s, /--- slide 2 ---\nSecond/);
  assert.ok(s.indexOf('第一页') < s.indexOf('Second'));
  const only2 = out(await officeReadTool.execute({ path: p, pages: '2' }, ctx));
  assert.doesNotMatch(only2, /第一页/);
});

test('office_write: docx and xlsx round-trip through office_read', async () => {
  const md = '# 月报\n\n本月 **营收** 增长 12%。\n\n- 北京\n  - 朝阳\n\n| 名称 | 数量 |\n| --- | --- |\n| a\\|b | 2 |';
  const d = await officeWriteTool.execute({ path: 'out.docx', content: md }, ctx);
  assert.ok(!d.isError, out(d));
  const rd = out(await officeReadTool.execute({ path: 'out.docx' }, ctx));
  assert.match(rd, /^# 月报$/m);
  assert.match(rd, /营收 增长 12%/);
  assert.match(rd, /^ {2}- 朝阳$/m);
  assert.match(rd, /\| 名称 \| 数量 \|\n\| --- \| --- \|\n\| a\\\|b \| 2 \|/);

  const x = await officeWriteTool.execute(
    { path: 'out.xlsx', content: [{ name: '销售', rows: '城市\t金额\n北京\t1234.5' }, { name: 'Other', rows: 'k\tv' }] },
    ctx
  );
  assert.ok(!x.isError, out(x));
  const rx = out(await officeReadTool.execute({ path: 'out.xlsx' }, ctx));
  assert.match(rx, /^1\t城市\t金额$/m);
  assert.match(rx, /^2\t北京\t1234\.5$/m);
  assert.match(rx, /## Other\n1\tk\tv/);

  assert.match(out(await officeWriteTool.execute({ path: 'bad.txt', content: 'x' }, ctx)), /\.docx and \.xlsx/);
  assert.match(out(await officeWriteTool.execute({ path: 'bad2.xlsx', content: 'a\nb\nc\nd\ne\nf' }, ctx)), /tab-separated/);
});

test('errors: unsupported types, old binaries, wrong container, missing file', async () => {
  writeFileSync(join(ctx.cwd, 'x.xls'), 'x');
  writeFileSync(join(ctx.cwd, 'x.zip'), 'x');
  writeFileSync(join(ctx.cwd, 'fake.docx'), zip({ 'other.xml': '<a/>' }));
  assert.match(out(await officeReadTool.execute({ path: 'x.xls' }, ctx)), /save it as \.xlsx/);
  assert.match(out(await officeReadTool.execute({ path: 'x.zip' }, ctx)), /unsupported file type/);
  assert.match(out(await officeReadTool.execute({ path: 'fake.docx' }, ctx)), /not a \.docx/);
  const missing = await officeReadTool.execute({ path: 'nope.pdf' }, ctx);
  assert.ok(missing.isError);
});
