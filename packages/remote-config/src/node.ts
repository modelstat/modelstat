/**
 * Node disk cache — the one piece the extension's in-memory model lacks.
 *
 * The extension's service worker reboots constantly, so it always
 * re-bootstraps from the bundled default; that is fine for a browser but
 * wrong for a long-lived daemon. A daemon that restarts while offline must
 * come back on the last config it fetched, not the shipped baseline. So we
 * persist each kind's payload to disk, next to identity.json.
 *
 * Layout (`~/.modelstat/config/`):
 *   {kind}.json   the last good config payload (human-readable JSON)
 *
 * Writes are atomic (tmp + rename), mirroring identity.ts and the daemon
 * status writer. The payload is re-validated on read by the loader, so a
 * torn, missing, or tampered file simply reads back as "no cache" and the
 * loader falls through — never a hard failure.
 */

import { chmod, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { CacheStore } from "./types.js";

function defaultRoot(): string {
  return join(homedir(), ".modelstat", "config");
}

/** Kinds are compiled-in slugs, but this is the value that names a file,
 * so refuse anything that could escape the cache directory. */
function safeKind(kind: string): string {
  if (!/^[a-z0-9_-]{1,64}$/.test(kind)) throw new Error(`unsafe config kind: ${kind}`);
  return kind;
}

export interface NodeDiskCacheOptions {
  /** Override the cache directory. Defaults to ~/.modelstat/config. */
  dir?: string;
}

export function createNodeDiskCache(opts: NodeDiskCacheOptions = {}): CacheStore {
  const dir = opts.dir ?? defaultRoot();

  async function writeAtomic(path: string, data: string): Promise<void> {
    const tmp = `${path}.${process.pid}.tmp`;
    await writeFile(tmp, data, { mode: 0o600 });
    await rename(tmp, path);
    try {
      await chmod(path, 0o600);
    } catch {
      /* best effort — rename already preserved tmp's mode on most platforms */
    }
  }

  return {
    async read(kind: string): Promise<unknown | null> {
      const k = safeKind(kind);
      try {
        const raw = await readFile(join(dir, `${k}.json`), "utf8");
        return JSON.parse(raw);
      } catch {
        return null;
      }
    },

    async write(kind: string, payload: unknown): Promise<void> {
      const k = safeKind(kind);
      await mkdir(dir, { recursive: true, mode: 0o700 });
      await writeAtomic(join(dir, `${k}.json`), `${JSON.stringify(payload)}\n`);
    },
  };
}
