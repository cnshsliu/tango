/**
 * Image attachments: local file paths → base64 ImageBlocks for the model.
 * Shared by the read tool and pasted-image prompts (serve `images` field).
 */
import { readFile, stat } from 'node:fs/promises';
import { resolve } from 'node:path';
import type { ImageBlock } from './types.js';

export const IMAGE_MIME: Record<string, string> = {
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp'
};

/** max raw bytes per image; base64 inflates ~4/3, and vendor caps sit around 5–20MB */
export const MAX_IMAGE_BYTES = 10 * 1024 * 1024;

export function imageMimeFor(path: string): string | undefined {
  const dot = path.lastIndexOf('.');
  if (dot < 0) return undefined;
  return IMAGE_MIME[path.slice(dot).toLowerCase()];
}

/**
 * Load image files into ImageBlocks. Paths resolve against cwd. Throws a friendly
 * error on the first bad path (missing file, unsupported format, too large) so the
 * UI can show it instead of sending a broken request.
 */
export async function loadImages(paths: string[], cwd: string): Promise<ImageBlock[]> {
  const blocks: ImageBlock[] = [];
  for (const raw of paths) {
    const p = resolve(cwd, raw);
    const mimeType = imageMimeFor(p);
    if (!mimeType) {
      throw new Error(`不支持的图片格式：${raw}（支持 png / jpg / jpeg / gif / webp）`);
    }
    const info = await stat(p).catch(() => null);
    if (!info?.isFile()) throw new Error(`图片不存在或不是文件：${raw}`);
    if (info.size > MAX_IMAGE_BYTES) {
      throw new Error(`图片过大：${raw}（${(info.size / 1024 / 1024).toFixed(1)}MB，上限 ${MAX_IMAGE_BYTES / 1024 / 1024}MB）`);
    }
    const data = (await readFile(p)).toString('base64');
    blocks.push({ type: 'image', mimeType, data, path: p });
  }
  return blocks;
}