/**
 * rename() that waits out Windows. There a rename fails with EPERM while another process — a
 * reader, a virus scanner, a concurrent writer of the same registry — holds the target or the
 * temp file open. On macOS and Linux the same rename simply replaces the target. Such a
 * failure is a moment, not a verdict: a few short retries with a growing wait, then the error
 * as it was.
 */
import { rename } from 'node:fs/promises';

const TRANSIENT = new Set(['EPERM', 'EBUSY', 'EACCES', 'ENOTEMPTY']);

export async function renameRetry(from: string, to: string, attempts = 8): Promise<void> {
  for (let i = 0; ; i++) {
    try {
      return await rename(from, to);
    } catch (e: any) {
      if (i >= attempts - 1 || !TRANSIENT.has(e?.code)) throw e;
      await new Promise((r) => setTimeout(r, 10 * (i + 1)));
    }
  }
}
