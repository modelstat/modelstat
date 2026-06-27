/**
 * Stable per-machine key — the permanent anchor for device identity.
 *
 * The whole point: the SAME physical machine must derive the SAME
 * device identity no matter how the daemon got installed (npm / pnpm /
 * bun / manual binary copy), across daemon-version bumps, across OS
 * upgrades, and EVEN after `~/.modelstat/` is deleted. Before this,
 * `deviceUuid` was a random UUIDv7 stored only in identity.json — so
 * any loss or reset of that file minted a brand-new UUID and the
 * server (which dedupes on device_uuid) created a duplicate device row.
 * That's how one Mac became three dashboard entries.
 *
 * The anchor is an OS/hardware identifier that lives OUTSIDE anything
 * we install:
 *   - macOS:   IOPlatformUUID (ioreg) — per-logic-board, survives OS
 *              reinstall and home-dir wipes.
 *   - Linux:   /etc/machine-id (systemd) or /var/lib/dbus/machine-id.
 *   - Windows: HKLM\SOFTWARE\Microsoft\Cryptography MachineGuid.
 *   - Fallback: a random key persisted at ~/.modelstat/machine-key
 *              (chmod 600) — at least stable per home dir when no
 *              hardware id is readable (containers, locked-down envs).
 *
 * We never send the raw hardware id off the machine: `machineKey()`
 * returns a salted SHA-256 of it. That hash is what we put in
 * `fingerprint.machine_id` (server dedupe key) and what we feed into
 * the deterministic device UUID, so the raw OS id can't be reversed
 * out of anything that leaves the device.
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { arch as cpuArch, homedir, hostname, platform, release } from "node:os";
import { join } from "node:path";

// Substituted by tsup's `define` at build time (see tsup.config.ts). A string
// literal like "daemon-0.0.33" in the bundle; "daemon-dev" when run unbundled
// (tsx / tests). Mirrors the macro cli.ts / daemon.ts / scan.ts use — declared
// here so buildFingerprint can stamp the version without importing from cli.ts.
const DAEMON_VERSION: string =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "daemon-dev";

/** Domain-separation salt so our hash can't be cross-referenced with
 * any other product that hashes the same OS machine id. Baked-in, not
 * secret — it just namespaces the digest. */
const MACHINE_KEY_SALT = "modelstat.device.machine-key.v1";

/** Fixed namespace for UUIDv5 device-id derivation (a random,
 * permanently-frozen UUID; changing it would re-key every device). */
const DEVICE_UUID_NAMESPACE = "6f1d2c9a-8b3e-4a7f-9c2d-0e5a1b6c7d8e";

const ROOT = join(homedir(), ".modelstat");
const FALLBACK_KEY_FILE = join(ROOT, "machine-key");

let cachedKey: string | null = null;
let cachedSource: MachineKeySource | null = null;

export type MachineKeySource =
  | "macos-ioplatform"
  | "linux-machine-id"
  | "windows-guid"
  | "fallback-file";

/** macOS: pull IOPlatformUUID out of ioreg. Returns null if ioreg is
 * unavailable or the field is missing. */
function macosPlatformUuid(): string | null {
  try {
    const r = spawnSync("ioreg", ["-rd1", "-c", "IOPlatformExpertDevice"], {
      encoding: "utf8",
      timeout: 4000,
    });
    if (r.status !== 0 || !r.stdout) return null;
    const m = /"IOPlatformUUID"\s*=\s*"([^"]+)"/.exec(r.stdout);
    return m?.[1]?.trim() || null;
  } catch {
    return null;
  }
}

/** Linux: systemd machine-id, then the dbus fallback path. */
function linuxMachineId(): string | null {
  for (const p of ["/etc/machine-id", "/var/lib/dbus/machine-id"]) {
    try {
      const v = readFileSync(p, "utf8").trim();
      if (v) return v;
    } catch {
      /* try next */
    }
  }
  return null;
}

/** Windows: MachineGuid from the registry. */
function windowsMachineGuid(): string | null {
  try {
    const r = spawnSync(
      "reg",
      ["query", "HKLM\\SOFTWARE\\Microsoft\\Cryptography", "/v", "MachineGuid"],
      { encoding: "utf8", timeout: 4000 },
    );
    if (r.status !== 0 || !r.stdout) return null;
    const m = /MachineGuid\s+REG_SZ\s+([0-9a-fA-F-]+)/.exec(r.stdout);
    return m?.[1]?.trim() || null;
  } catch {
    return null;
  }
}

/** Read (or lazily create) the persisted random fallback key. This is
 * the last resort when no hardware id is readable; it survives reinstalls
 * but NOT a `~/.modelstat` wipe, so it's strictly worse than a hardware
 * id — hence only used when the platform probes come up empty. */
function fallbackKey(): string {
  try {
    if (existsSync(FALLBACK_KEY_FILE)) {
      const v = readFileSync(FALLBACK_KEY_FILE, "utf8").trim();
      if (v) return v;
    }
  } catch {
    /* fall through to (re)create */
  }
  const fresh = createHash("sha256")
    .update(`${MACHINE_KEY_SALT}:fallback:${Date.now()}:${Math.random()}`)
    .digest("hex");
  try {
    mkdirSync(ROOT, { recursive: true, mode: 0o700 });
    writeFileSync(FALLBACK_KEY_FILE, fresh, { mode: 0o600 });
    chmodSync(FALLBACK_KEY_FILE, 0o600);
  } catch {
    /* best effort — even an unpersisted key is stable for this process */
  }
  return fresh;
}

/** Resolve the raw machine id + which source produced it. */
function resolveRaw(): { raw: string; source: MachineKeySource } {
  const p = platform();
  if (p === "darwin") {
    const v = macosPlatformUuid();
    if (v) return { raw: v, source: "macos-ioplatform" };
  } else if (p === "win32") {
    const v = windowsMachineGuid();
    if (v) return { raw: v, source: "windows-guid" };
  } else {
    const v = linuxMachineId();
    if (v) return { raw: v, source: "linux-machine-id" };
  }
  return { raw: fallbackKey(), source: "fallback-file" };
}

/**
 * The stable machine key: a salted SHA-256 (64-char hex) of the raw
 * hardware/OS machine id. Memoised for the process. Safe to send to the
 * server (the raw id is never exposed). Used as `fingerprint.machine_id`.
 */
export function machineKey(): string {
  if (cachedKey) return cachedKey;
  const { raw, source } = resolveRaw();
  cachedSource = source;
  cachedKey = createHash("sha256").update(`${MACHINE_KEY_SALT}:${raw}`).digest("hex");
  return cachedKey;
}

/** Which source the cached key came from (computes the key if needed).
 * Exposed for `paths`/diagnostics so a user can see whether they're on a
 * hardware anchor or the weaker file fallback. */
export function machineKeySource(): MachineKeySource {
  if (!cachedSource) machineKey();
  return cachedSource!;
}

/**
 * Deterministic device UUID (RFC 9562 v5) derived from a machine key.
 * Same key → same UUID, always — so a machine that lost its
 * identity.json re-derives the exact UUID it had, and the server maps
 * it back to the existing device row instead of minting a duplicate.
 *
 * Standard UUIDv5: SHA-1(namespace_bytes ++ name) → first 16 bytes,
 * with the version (5) and variant (10xx) bits forced.
 */
export function deviceUuidFromMachineKey(key: string = machineKey()): string {
  const nsHex = DEVICE_UUID_NAMESPACE.replace(/-/g, "");
  const nsBytes = Buffer.from(nsHex, "hex");
  const hash = createHash("sha1").update(nsBytes).update(key).digest();
  const b = Buffer.from(hash.subarray(0, 16));
  b[6] = (b[6]! & 0x0f) | 0x50; // version 5
  b[8] = (b[8]! & 0x3f) | 0x80; // variant 10
  const h = b.toString("hex");
  return [h.slice(0, 8), h.slice(8, 12), h.slice(12, 16), h.slice(16, 20), h.slice(20, 32)].join(
    "-",
  );
}

/**
 * The device UUID this machine SHOULD use, derived deterministically from
 * its stable hardware/OS machine key. Same machine → same UUID forever, so a
 * machine that lost `~/.modelstat` re-derives the exact UUID it had and the
 * server maps it back to the existing device row instead of minting a
 * duplicate.
 *
 * Escape hatch: set MODELSTAT_DEVICE_SALT to run a SECOND logical device on
 * one machine (CI matrices, multi-tenant boxes). It's a deterministic salt,
 * not randomness — so even that stays idempotent across reinstalls. Absent it,
 * one machine is exactly one device.
 *
 * Lives here (not in cli.ts) alongside `machineKey()` so the machine_id /
 * device-UUID derivation can never drift between the register path, the
 * heartbeat path, and the machine-stable recovery path — they all read the
 * same source.
 */
export function intendedDeviceUuid(): string {
  const salt = process.env.MODELSTAT_DEVICE_SALT?.trim();
  const key = salt ? `${machineKey()}:${salt}` : machineKey();
  return deviceUuidFromMachineKey(key);
}

function osFamily(): "macos" | "linux" | "other" {
  const p = platform();
  if (p === "darwin") return "macos";
  if (p === "linux") return "linux";
  return "other";
}

function osArch(): "x86_64" | "arm64" | "other" {
  const a = cpuArch();
  if (a === "x64") return "x86_64";
  if (a === "arm64") return "arm64";
  return "other";
}

/**
 * The single device fingerprint, shared by BOTH register (`POST /v1/tokens`)
 * and heartbeat. `fingerprint.machine_id` is the server's dedupe anchor — the
 * one field that maps this physical machine back to its existing device row —
 * so it MUST be byte-identical everywhere it's sent. Factoring it here means
 * register and heartbeat can't disagree on it (the bug that turned one Mac
 * into three dashboard rows).
 */
export function buildFingerprint(): Record<string, string | number | boolean> {
  return {
    hostname: hostname(),
    os_family: osFamily(),
    os_version: release(),
    arch: osArch(),
    daemon: "modelstat-daemon",
    daemon_version: DAEMON_VERSION,
    // Stable, install-method-independent machine key — see machineKey(). The
    // server dedupes on this so the same physical machine can never become two
    // device rows, even if the UUID somehow differs (legacy random UUID →
    // deterministic UUID transition).
    machine_id: machineKey(),
  };
}
