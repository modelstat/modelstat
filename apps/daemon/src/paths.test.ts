import { strict as assert } from "node:assert";
import { homedir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { homePath, modelstatHome } from "./paths.js";

function withEnv(value: string | undefined, fn: () => void): void {
  const saved = process.env.MODELSTAT_HOME;
  if (value === undefined) delete process.env.MODELSTAT_HOME;
  else process.env.MODELSTAT_HOME = value;
  try {
    fn();
  } finally {
    if (saved === undefined) delete process.env.MODELSTAT_HOME;
    else process.env.MODELSTAT_HOME = saved;
  }
}

test("modelstatHome defaults to ~/.modelstat", () => {
  withEnv(undefined, () => {
    assert.equal(modelstatHome(), join(homedir(), ".modelstat"));
    assert.equal(homePath("state.json"), join(homedir(), ".modelstat", "state.json"));
  });
});

test("MODELSTAT_HOME relocates everything (e.g. /opt/modelstat)", () => {
  withEnv("/opt/modelstat", () => {
    assert.equal(modelstatHome(), "/opt/modelstat");
    assert.equal(homePath("identity.json"), "/opt/modelstat/identity.json");
    assert.equal(homePath("state.json"), "/opt/modelstat/state.json");
  });
});

test("blank / whitespace MODELSTAT_HOME falls back to the default", () => {
  withEnv("   ", () => {
    assert.equal(modelstatHome(), join(homedir(), ".modelstat"));
  });
});
