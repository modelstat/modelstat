import { strict as assert } from "node:assert";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { runtimeState, statePath } from "./runtime-state.js";

/** Run `fn` with MODELSTAT_HOME pointed at a throwaway dir + a clean cache. */
function withTempHome(fn: () => void): void {
  const saved = process.env.MODELSTAT_HOME;
  const dir = mkdtempSync(join(tmpdir(), "modelstat-state-"));
  process.env.MODELSTAT_HOME = dir;
  runtimeState._resetCacheForTests();
  try {
    fn();
  } finally {
    runtimeState._resetCacheForTests();
    if (saved === undefined) delete process.env.MODELSTAT_HOME;
    else process.env.MODELSTAT_HOME = saved;
    rmSync(dir, { recursive: true, force: true });
  }
}

test("statePath lives under MODELSTAT_HOME", () => {
  withTempHome(() => {
    assert.ok(statePath().endsWith("/state.json"));
    assert.ok(statePath().startsWith(process.env.MODELSTAT_HOME ?? ""));
  });
});

test("apiUrl / cursors / processingVersion / segmentsSent round-trip through disk", () => {
  withTempHome(() => {
    runtimeState.setApiUrl("https://example.test");
    runtimeState.setCursor("/a/b.jsonl", { size: 10, mtime: 1, tailHash: "h" });
    runtimeState.setProcessingVersion(6);
    runtimeState.bumpSegmentsSent(3);

    runtimeState._resetCacheForTests(); // force a fresh disk read
    assert.equal(runtimeState.getApiUrl(), "https://example.test");
    assert.deepEqual(runtimeState.getCursor("/a/b.jsonl"), { size: 10, mtime: 1, tailHash: "h" });
    assert.equal(runtimeState.getProcessingVersion(), 6);
    assert.equal(runtimeState.getSegmentsSent(), 3);

    assert.ok(existsSync(statePath()));
    const onDisk = JSON.parse(readFileSync(statePath(), "utf8"));
    assert.equal(onDisk.processingVersion, 6);
  });
});

test("missing state file ⇒ defaults (fresh install / relocation)", () => {
  withTempHome(() => {
    assert.equal(runtimeState.getProcessingVersion(), null);
    assert.equal(runtimeState.getCursor("/nope"), undefined);
    assert.equal(runtimeState.getApiUrl(), "");
    assert.equal(runtimeState.getSegmentsSent(), 0);
    assert.deepEqual(runtimeState.getReshipState(), {});
  });
});

test("reshipState (self-heal backoff bookkeeping) round-trips through disk", () => {
  withTempHome(() => {
    runtimeState.setReshipState({ "2026-06-01\0sess-1": { attempts: 2, lastAt: 1234 } });
    runtimeState._resetCacheForTests();
    assert.deepEqual(runtimeState.getReshipState(), {
      "2026-06-01\0sess-1": { attempts: 2, lastAt: 1234 },
    });
    const onDisk = JSON.parse(readFileSync(statePath(), "utf8"));
    assert.equal(onDisk.reshipState["2026-06-01\0sess-1"].attempts, 2);
  });
});

test("wipeCursors clears every cursor (rescan / version bump)", () => {
  withTempHome(() => {
    runtimeState.setCursor("/x", { size: 1, mtime: 1, tailHash: "a" });
    runtimeState.setCursor("/y", { size: 2, mtime: 2, tailHash: "b" });
    runtimeState.wipeCursors();
    runtimeState._resetCacheForTests();
    assert.equal(runtimeState.getCursor("/x"), undefined);
    assert.equal(runtimeState.getCursor("/y"), undefined);
  });
});
