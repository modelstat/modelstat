/**
 * Acceptance tests for the additive `policies` augment:
 *   • a signed bundle adds a pattern fleet-wide with no release;
 *   • the floor still applies with no augment (offline);
 *   • a (signed) bundle can never weaken the floor — structurally there is no
 *     "remove" field, and behaviourally the baseline still fires.
 */

import { strict as assert } from "node:assert";
import { afterEach, test } from "node:test";
import {
  compilePolicyPatterns,
  POLICIES_BUNDLED_FALLBACK,
  RedactionPolicyBundle,
} from "./policies.js";
import { clearRemoteRedactionPatterns, redact, setRemoteRedactionPatterns } from "./redact.js";

afterEach(() => clearRemoteRedactionPatterns());

test("a signed policies augment adds a pattern fleet-wide (no release)", () => {
  const probe = "vendor token acme_ABCDEFGHIJKLMNOPQRSTUV here";
  // Before: an unknown vendor's token format slips past the floor.
  assert.equal(redact(probe).counts.secrets_found, 0);

  const bundle = RedactionPolicyBundle.parse({
    version: 3,
    patterns: [{ name: "acme_api_key", regex: "acme_[A-Za-z0-9]{20,}" }],
  });
  setRemoteRedactionPatterns(compilePolicyPatterns(bundle));

  const r = redact(probe);
  assert.match(r.text, /\[REDACTED:acme_api_key\]/);
  assert.equal(r.counts.secrets_found, 1);
});

test("the floor still applies with no augment (offline / bundled fallback)", () => {
  setRemoteRedactionPatterns(compilePolicyPatterns(POLICIES_BUNDLED_FALLBACK));
  const r = redact("key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789");
  assert.equal(r.counts.secrets_found, 1);
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
});

test("a hostile augment CANNOT weaken the floor (behavioural)", () => {
  // A bundle full of useless noise is still applied — but the baseline floor
  // fires regardless, so real secrets are still redacted.
  setRemoteRedactionPatterns(
    compilePolicyPatterns(
      RedactionPolicyBundle.parse({
        version: 9,
        patterns: [{ name: "noise", regex: "zzzz_nope" }],
      }),
    ),
  );
  const r = redact("sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789 then AKIAIOSFODNN7EXAMPLE");
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
  assert.match(r.text, /\[REDACTED:aws_access_key\]/);
});

test("the bundle schema has no way to express removal (structural)", () => {
  // A bundle that *tries* to disable or remove floor patterns parses down to
  // exactly {version, patterns} — the hostile keys are dropped by the schema.
  const parsed = RedactionPolicyBundle.parse({
    version: 1,
    patterns: [],
    disableFloor: true,
    removePatterns: ["anthropic_key"],
    floor: [],
  });
  assert.deepEqual(Object.keys(parsed).sort(), ["patterns", "version"]);
});

test("invalid remote regexes are skipped, never thrown", () => {
  const compiled = compilePolicyPatterns(
    RedactionPolicyBundle.parse({
      version: 1,
      patterns: [
        { name: "good", regex: "good_[0-9]+" },
        { name: "bad", regex: "(" }, // un-compilable — must be skipped
      ],
    }),
  );
  assert.equal(compiled.length, 1);
  assert.equal(compiled[0]?.name, "good");
});

test("pattern names are constrained so they can't corrupt the placeholder", () => {
  const bad = RedactionPolicyBundle.safeParse({
    version: 1,
    patterns: [{ name: "evil]name", regex: "x+" }],
  });
  assert.equal(bad.success, false);
});
