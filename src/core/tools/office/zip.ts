import { inflateRawSync } from 'node:zlib';

const MAX_ENTRY_BYTES = 200 * 1024 * 1024;
const U32 = 0xffffffff;

interface Entry {
  method: number;
  flags: number;
  offset: number;
  compSize: number;
  size: number;
}

/** Minimal read-only ZIP reader (stored + deflate, zip64), enough for OOXML containers. */
export class Zip {
  private entries = new Map<string, Entry>();

  constructor(private buf: Buffer) {
    const eocd = findEocd(buf);
    let count = buf.readUInt16LE(eocd + 10);
    let p = buf.readUInt32LE(eocd + 16);
    if (p === U32 || count === 0xffff) {
      const loc = eocd - 20;
      if (loc < 0 || buf.readUInt32LE(loc) !== 0x07064b50) throw new Error('corrupt zip (zip64 locator missing)');
      const z64 = Number(buf.readBigUInt64LE(loc + 8));
      if (buf.readUInt32LE(z64) !== 0x06064b50) throw new Error('corrupt zip (zip64 record)');
      count = Number(buf.readBigUInt64LE(z64 + 32));
      p = Number(buf.readBigUInt64LE(z64 + 48));
    }
    for (let i = 0; i < count; i++) {
      if (p + 46 > buf.length || buf.readUInt32LE(p) !== 0x02014b50) throw new Error('corrupt zip (central directory)');
      const flags = buf.readUInt16LE(p + 8);
      const method = buf.readUInt16LE(p + 10);
      let compSize = buf.readUInt32LE(p + 20);
      let size = buf.readUInt32LE(p + 24);
      const nameLen = buf.readUInt16LE(p + 28);
      const extraLen = buf.readUInt16LE(p + 30);
      const commentLen = buf.readUInt16LE(p + 32);
      let offset = buf.readUInt32LE(p + 42);
      const name = buf.toString('utf8', p + 46, p + 46 + nameLen).replace(/\\/g, '/');
      if (size === U32 || compSize === U32 || offset === U32) {
        // zip64 extra field: only the overflowed values are present, in this order
        for (let e = p + 46 + nameLen, end = e + extraLen; e + 4 <= end; e += 4 + buf.readUInt16LE(e + 2)) {
          if (buf.readUInt16LE(e) !== 1) continue;
          let q = e + 4;
          if (size === U32) (size = Number(buf.readBigUInt64LE(q))), (q += 8);
          if (compSize === U32) (compSize = Number(buf.readBigUInt64LE(q))), (q += 8);
          if (offset === U32) offset = Number(buf.readBigUInt64LE(q));
          break;
        }
      }
      this.entries.set(name, { method, flags, offset, compSize, size });
      p += 46 + nameLen + extraLen + commentLen;
    }
  }

  names(): string[] {
    return [...this.entries.keys()];
  }

  has(name: string): boolean {
    return this.entries.has(name);
  }

  read(name: string): Buffer | undefined {
    const e = this.entries.get(name);
    if (!e) return undefined;
    if (e.flags & 1) throw new Error(`${name} is encrypted`);
    if (this.buf.readUInt32LE(e.offset) !== 0x04034b50) throw new Error(`corrupt zip (local header of ${name})`);
    const start = e.offset + 30 + this.buf.readUInt16LE(e.offset + 26) + this.buf.readUInt16LE(e.offset + 28);
    const data = this.buf.subarray(start, start + e.compSize);
    if (e.method === 0) return Buffer.from(data);
    if (e.method === 8) return inflateRawSync(data, { maxOutputLength: MAX_ENTRY_BYTES });
    throw new Error(`${name}: unsupported zip compression method ${e.method}`);
  }

  text(name: string): string | undefined {
    const b = this.read(name);
    return b && b.toString('utf8').replace(/^\uFEFF/, '');
  }
}

function findEocd(buf: Buffer): number {
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 65557); i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) return i;
  }
  throw new Error('not a zip file');
}
