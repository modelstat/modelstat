import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { autoUpdateEnabled, autoUpdatePinnedByEnv, maybeAutoUpdate } from "./update.js";

describe("update", () => {
  it("env override pins auto-update off", () => {
    process.env.MODELSTAT_AUTO_UPDATE = "off";
    assert.equal(autoUpdateEnabled(), false);
    assert.equal(autoUpdatePinnedByEnv(), true);
    delete process.env.MODELSTAT_AUTO_UPDATE;
    assert.equal(autoUpdatePinnedByEnv(), false);
  });

  it("ok verdict is a no-op (no spawn, no note)", () => {
    assert.equal(maybeAutoUpdate("ok", null), null);
    assert.equal(maybeAutoUpdate("anything-else", "1.0.0"), null);
  });

  it("nudges without spawning when auto-update is off, deduped per (verdict,target)", () => {
    process.env.MODELSTAT_AUTO_UPDATE = "off";
    // Unique target so this test's dedup state can't collide with others.
    const first = maybeAutoUpdate("upgrade_required", "9.9.9");
    assert.match(first ?? "", /auto-update is off/);
    assert.match(first ?? "", /upgrade required/);
    // Same (verdict,target) again → deduped to null (no repeat log / no spawn).
    assert.equal(maybeAutoUpdate("upgrade_required", "9.9.9"), null);
    delete process.env.MODELSTAT_AUTO_UPDATE;
  });
});
