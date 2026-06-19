/**
 * Regression tests for daemon supervision (supervise.ts + lock.ts).
 *
 * Pins the rules that prevent the 2026-06-12 failure mode — two tray
 * instances whose daemons SIGTERM each other via blind `start --force`
 * until zero daemons survive:
 *   1. A live, heartbeating lock owner is ADOPTED, never killed.
 *   2. A dead owner means plain spawn (stale locks need no --force).
 *   3. Only a live-but-silent (wedged) owner gets replaced.
 *   4. A daemon that loses the lock-write race stands down ("lost")
 *      instead of running as a silent duplicate.
 */

import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { checkLockOwnership, type LockMeta } from "./lock.js";
import { BOOT_GRACE_MS, daemonHealth, decideSupervision, STATUS_FRESH_MS } from "./supervise.js";

function lockMeta(pid: number, startedAtMsAgo: number, now: number): LockMeta {
  return {
    pid,
    startedAt: new Date(now - startedAtMsAgo).toISOString(),
    companionVersion: "test",
    apiUrl: "http://localhost",
  };
}

const NOW = Date.parse("2026-06-12T12:00:00Z");

test("decideSupervision: no lock → spawn", () => {
  assert.equal(
    decideSupervision({ lock: null, ownerAlive: false, lockAgeMs: null, statusAgeMs: 5_000 }),
    "spawn",
  );
});

test("decideSupervision: dead owner → spawn (stale lock, no --force needed)", () => {
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, 600_000, NOW),
      ownerAlive: false,
      lockAgeMs: 600_000,
      statusAgeMs: 600_000,
    }),
    "spawn",
  );
});

test("decideSupervision: live owner with fresh heartbeat → adopt (the anti-murder-loop rule)", () => {
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, 3_600_000, NOW),
      ownerAlive: true,
      lockAgeMs: 3_600_000,
      statusAgeMs: 9_000, // one heartbeat ago
    }),
    "adopt",
  );
});

test("decideSupervision: live owner, stale heartbeat, but freshly booted → adopt (grace)", () => {
  // The status file still belongs to the PREVIOUS daemon; the new one
  // hasn't written yet. Killing it here is exactly the dying-tray
  // counter-kill that ended with zero daemons.
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, 5_000, NOW),
      ownerAlive: true,
      lockAgeMs: 5_000,
      statusAgeMs: STATUS_FRESH_MS + 60_000,
    }),
    "adopt",
  );
});

test("decideSupervision: live owner, silent past grace → replace (wedged)", () => {
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, BOOT_GRACE_MS + 60_000, NOW),
      ownerAlive: true,
      lockAgeMs: BOOT_GRACE_MS + 60_000,
      statusAgeMs: STATUS_FRESH_MS + 60_000,
    }),
    "replace",
  );
});

test("decideSupervision: healthy owner on a DIFFERENT daemon version → replace (upgrade path)", () => {
  // After an upgrade re-stages the bundle, the old-version daemon
  // survives launchd's group kill and heartbeats forever; adopting it
  // would pin the old code. Version drift wins over freshness.
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, 3_600_000, NOW),
      ownerAlive: true,
      lockAgeMs: 3_600_000,
      statusAgeMs: 1_000,
      myCompanionVersion: "daemon-0.0.43",
    }),
    "replace",
  );
  // Same version → freshness rules apply as usual.
  const sameVersion = { ...lockMeta(4242, 3_600_000, NOW), companionVersion: "daemon-0.0.43" };
  assert.equal(
    decideSupervision({
      lock: sameVersion,
      ownerAlive: true,
      lockAgeMs: 3_600_000,
      statusAgeMs: 1_000,
      myCompanionVersion: "daemon-0.0.43",
    }),
    "adopt",
  );
  // Unknown lock version (pre-versioning lockfile) → don't churn.
  const unknownVersion = { ...lockMeta(4242, 3_600_000, NOW), companionVersion: "unknown" };
  assert.equal(
    decideSupervision({
      lock: unknownVersion,
      ownerAlive: true,
      lockAgeMs: 3_600_000,
      statusAgeMs: 1_000,
      myCompanionVersion: "daemon-0.0.43",
    }),
    "adopt",
  );
});

test("decideSupervision: live owner, no status file at all, old lock → replace", () => {
  assert.equal(
    decideSupervision({
      lock: lockMeta(4242, BOOT_GRACE_MS + 60_000, NOW),
      ownerAlive: true,
      lockAgeMs: BOOT_GRACE_MS + 60_000,
      statusAgeMs: null,
    }),
    "replace",
  );
});

test("daemonHealth: reads lock + status from disk and decides (temp fixtures)", async () => {
  const dir = await mkdtemp(join(tmpdir(), "modelstat-supervise-"));
  try {
    const lockPath = join(dir, "daemon.lock");
    const statusPath = join(dir, "last-status.json");

    // Live owner (this test process), heartbeat 5s old → adopt.
    await writeFile(lockPath, JSON.stringify(lockMeta(process.pid, 60_000, NOW)));
    await writeFile(
      statusPath,
      JSON.stringify({ written_at: new Date(NOW - 5_000).toISOString() }),
    );
    let h = daemonHealth({ lockPath, statusPath, now: NOW });
    assert.equal(h.decision, "adopt");
    assert.equal(h.ownerAlive, true);
    assert.equal(h.statusAgeMs, 5_000);

    // Same fixtures but the owner pid probes dead → spawn.
    h = daemonHealth({ lockPath, statusPath, now: NOW, pidAlive: () => false });
    assert.equal(h.decision, "spawn");

    // Old lock + stale status + live owner → replace.
    await writeFile(lockPath, JSON.stringify(lockMeta(process.pid, BOOT_GRACE_MS + 120_000, NOW)));
    await writeFile(
      statusPath,
      JSON.stringify({ written_at: new Date(NOW - STATUS_FRESH_MS - 120_000).toISOString() }),
    );
    h = daemonHealth({ lockPath, statusPath, now: NOW });
    assert.equal(h.decision, "replace");

    // Missing lock file → spawn.
    await rm(lockPath);
    h = daemonHealth({ lockPath, statusPath, now: NOW });
    assert.equal(h.decision, "spawn");
    assert.equal(h.lock, null);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("checkLockOwnership: rename-race loser stands down only against a LIVE winner", () => {
  const me = 1000;
  const winner = lockMeta(2000, 0, NOW);
  // Lock shows us (or vanished) → owned.
  assert.equal(
    checkLockOwnership(me, lockMeta(me, 0, NOW), () => true),
    "owned",
  );
  assert.equal(
    checkLockOwnership(me, null, () => true),
    "owned",
  );
  // Live rival owns it → lost (stand down, supervisor adopts them).
  assert.equal(
    checkLockOwnership(me, winner, () => true),
    "lost",
  );
  // Rival already died → keep running (no thrash).
  assert.equal(
    checkLockOwnership(me, winner, () => false),
    "winner_dead",
  );
});
