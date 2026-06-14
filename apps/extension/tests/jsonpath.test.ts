import { describe, expect, it } from "vitest";
import { asInt, asString, jsonPath } from "../src/interpreter/jsonpath.js";

describe("jsonPath", () => {
  it("returns root for $", () => {
    expect(jsonPath({ a: 1 }, "$")).toEqual({ a: 1 });
  });

  it("extracts child by name", () => {
    expect(jsonPath({ a: { b: 42 } }, "$.a.b")).toBe(42);
  });

  it("returns null on missing path", () => {
    expect(jsonPath({ a: {} }, "$.a.b.c")).toBeNull();
  });

  it("extracts array index", () => {
    expect(jsonPath({ xs: [10, 20, 30] }, "$.xs[1]")).toBe(20);
  });

  it("flattens array spread", () => {
    const doc = { parts: [{ t: "hello " }, { t: "world" }] };
    expect(jsonPath(doc, "$.parts[*].t")).toEqual(["hello ", "world"]);
  });

  it("handles nested spread → scalar path", () => {
    const doc = {
      messages: [
        { content: { parts: ["a", "b"] } },
        { content: { parts: ["c"] } },
      ],
    };
    expect(jsonPath(doc, "$.messages[*].content.parts[*]")).toEqual(["a", "b", "c"]);
  });

  it("supports bracket-string keys", () => {
    expect(jsonPath({ "weird key": 7 }, '$["weird key"]')).toBe(7);
  });

  it("asString joins arrays", () => {
    expect(asString(["hello ", "world"])).toBe("hello world");
  });

  it("asInt handles numeric strings", () => {
    expect(asInt("42")).toBe(42);
    expect(asInt("abc")).toBeNull();
    expect(asInt(3.7)).toBe(3);
  });
});
