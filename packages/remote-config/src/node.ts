/**
 * Node disk cache — the one piece the extension's adapter loader lacks.
 *
 * The extension's service worker reboots constantly, so it always
 * re-bootstraps from the bundled default; that is fine for a browser but
 * wrong for a long-lived daemon. A daemon that restarts while offline
 * must come back on the last *verified* bundle it fetched, not the
 * shipped baseline. So we persist each kind to disk, next to identity.json.
 *
 * Layout (`~/.modelstat/config/`):
 *   {kind}.json      the exact signed config bytes (human-readable JSON)
 *   {kind}.sig       base64 Ed25519 signature over those bytes
 *   {kind}.version   { "version": N, "signed_at": "…" }
 *
 * Writes are atomic (tmp + rename), mirroring identity.ts and the daemon
 * status writer. The cache is re-verified on read by the loader, so a
 * torn, missing, or tampered triple simply reads back as "no cache" and
 * the loader falls through to disk-less behavior — never a hard failure.
 */

import { chmod, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { base64ToBytes, bytesToBase64 } from "./crypto.js";
import type { CachedBundle, CacheStore } from "./types.js";

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

  async function writeAtomic(path: string, data: Uint8Array | string): Promise<void> {
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
    async read(kind: string): Promise<CachedBundle | null> {
      const k = safeKind(kind);
      try {
        const [configBytes, sig, versionRaw] = await Promise.all([
          readFile(join(dir, `${k}.json`)),
          readFile(join(dir, `${k}.sig`), "utf8"),
          readFile(join(dir, `${k}.version`), "utf8"),
        ]);
        const meta = JSON.parse(versionRaw) as { version?: unknown; signed_at?: unknown };
        if (typeof meta.version !== "number" || typeof meta.signed_at !== "string") return null;
        return {
          version: meta.version,
          signed_at: meta.signed_at,
          // Re-encode the exact on-disk bytes; the loader re-verifies them.
          config: bytesToBase64(new Uint8Array(configBytes)),
          signature: sig.trim(),
        };
      } catch {
        return null;
      }
    },

    async write(kind: string, bundle: CachedBundle): Promise<void> {
      const k = safeKind(kind);
      await mkdir(dir, { recursive: true, mode: 0o700 });
      // Persist the EXACT signed bytes (not a re-serialization) so the
      // cache re-verifies byte-for-byte on the next read.
      const configBytes = base64ToBytes(bundle.config);
      await writeAtomic(join(dir, `${k}.json`), configBytes);
      await writeAtomic(join(dir, `${k}.sig`), `${bundle.signature}\n`);
      await writeAtomic(
        join(dir, `${k}.version`),
        `${JSON.stringify({ version: bundle.version, signed_at: bundle.signed_at })}\n`,
      );
    },
  };
}
