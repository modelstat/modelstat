import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  COGNITION_SYSTEM_PROMPT,
  cognitionHints,
  EMPTY_COGNITION,
  formatCognitionSuffix,
  parseCognitionReply,
} from "./cognition.js";

describe("cognition prompt", () => {
  it("asks for emotions, meta, AND posture in the schema", () => {
    assert.ok(COGNITION_SYSTEM_PROMPT.includes('{"emotions":[],"meta":[],"posture":[]}'));
    assert.ok(COGNITION_SYSTEM_PROMPT.toLowerCase().includes("posture"));
    // soft vocab present so the model has examples but stays free to be creative
    assert.ok(COGNITION_SYSTEM_PROMPT.includes("ship-it"));
    assert.ok(COGNITION_SYSTEM_PROMPT.includes("cautious"));
  });
});

describe("parseCognitionReply — posture", () => {
  it("parses + sanitises a posture field alongside emotions/meta", () => {
    assert.deepEqual(
      parseCognitionReply(
        '{"emotions":["Frustrated!"],"meta":["focused"],"posture":["Ship-It","yolo"]}',
      ),
      { emotions: ["frustrated"], meta: ["focused"], posture: ["ship-it", "yolo"] },
    );
  });

  it("defaults posture to [] when the field is absent (older replies)", () => {
    assert.deepEqual(parseCognitionReply('{"emotions":["calm"],"meta":[]}'), {
      emotions: ["calm"],
      meta: [],
      posture: [],
    });
  });

  it("tolerates fenced / prose-wrapped JSON", () => {
    assert.deepEqual(
      parseCognitionReply('```json\n{"emotions":[],"meta":[],"posture":["cautious"]}\n```')
        ?.posture,
      ["cautious"],
    );
  });
});

describe("formatCognitionSuffix — stance", () => {
  it("renders a [Stance: …] segment after Mood/Mind", () => {
    assert.equal(
      formatCognitionSuffix({ emotions: ["calm"], meta: ["focused"], posture: ["cautious"] }),
      "[Mood: calm] [Mind: focused] [Stance: cautious]",
    );
  });

  it("omits empty fields entirely", () => {
    assert.equal(
      formatCognitionSuffix({ emotions: [], meta: [], posture: ["yolo"] }),
      "[Stance: yolo]",
    );
    assert.equal(formatCognitionSuffix(EMPTY_COGNITION), "");
  });
});

describe("cognitionHints — structured mood + posture hints", () => {
  it("emits the PRIMARY mood + posture, capitalised, as same-keyed hints", () => {
    assert.deepEqual(
      cognitionHints({
        emotions: ["frustrated", "curious"],
        meta: ["stuck"],
        posture: ["cautious", "questioning"],
      }),
      [
        { root_key: "mood", name: "Frustrated", confidence: 0.7 },
        { root_key: "posture", name: "Cautious", confidence: 0.7 },
      ],
    );
  });

  it("is a no-op when cognition is empty or null (real-data-only by construction)", () => {
    assert.deepEqual(cognitionHints(EMPTY_COGNITION), []);
    assert.deepEqual(cognitionHints(null), []);
    assert.deepEqual(cognitionHints(undefined), []);
  });

  it("emits only the dimension that has a tag", () => {
    assert.deepEqual(cognitionHints({ emotions: ["excited"], meta: [], posture: [] }), [
      { root_key: "mood", name: "Excited", confidence: 0.7 },
    ]);
    assert.deepEqual(cognitionHints({ emotions: [], meta: [], posture: ["ship-it"] }), [
      { root_key: "posture", name: "Ship-it", confidence: 0.7 },
    ]);
  });
});
