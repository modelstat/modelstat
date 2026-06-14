/**
 * Signer/verifier byte-compatibility proof.
 *
 * This golden vector is shared verbatim with the server's signing tests. The
 * signer reproduces this exact signature deterministically (Ed25519 is
 * deterministic, RFC 8032); here, the companion's WebCrypto verifier accepts
 * it. Together they prove the signer and this verifier agree byte-for-byte —
 * without ever sharing the production key.
 *
 * TEST-ONLY. The keypair is a fixed throwaway; never a production key.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { z } from "zod";
import { base64ToBytes, bytesToBase64, utf8ToBytes } from "./crypto.js";
import { verifySignedBundle } from "./verify.js";

const GOLDEN_PUBKEY_B64 = "oHn+BnHLUkc4vet6Qv30jxUedG6HALb63XkE+Z+15m4=";
const GOLDEN_PAYLOAD =
  '{"version":1,"patterns":[{"name":"acme_api_key","regex":"acme_[A-Za-z0-9]{20,}","label":"acme_api_key"}]}';
const GOLDEN_SIG_B64 =
  "BSxDC+AWRBeXdkKFcDUKjFe5pwXrWrEVxOmmEjQt/f4jsTcannxbEXgEOBhkBQirogTgJOi8idu+7w3eHCtgCg==";

const Policy = z.object({
  version: z.number().int().nonnegative(),
  patterns: z.array(
    z.object({ name: z.string(), regex: z.string(), label: z.string().optional() }),
  ),
});

function goldenEnvelope() {
  return {
    // The companion derives `config` from the same payload bytes the server
    // signed, so there is no long base64 to keep in sync across the two repos.
    config: bytesToBase64(utf8ToBytes(GOLDEN_PAYLOAD)),
    signature: GOLDEN_SIG_B64,
    signed_at: "2026-06-14T00:00:00.000Z",
    version: 1,
  };
}

test("golden vector: WebCrypto verifies the server-signed bundle (byte-compatible)", async () => {
  const result = await verifySignedBundle({
    envelope: goldenEnvelope(),
    publicKey: base64ToBytes(GOLDEN_PUBKEY_B64),
    schema: Policy,
  });
  assert.equal(result.ok, true);
  if (result.ok) {
    assert.equal(result.value.version, 1);
    assert.equal(result.value.patterns[0]?.name, "acme_api_key");
  }
});

test("golden vector: a different trust root rejects the signature", async () => {
  const pk = base64ToBytes(GOLDEN_PUBKEY_B64);
  pk[0] = pk[0]! ^ 0xff; // flip the first byte of the bundled key
  const result = await verifySignedBundle({
    envelope: goldenEnvelope(),
    publicKey: pk,
    schema: Policy,
  });
  assert.equal(result.ok, false);
});
