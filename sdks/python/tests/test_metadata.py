"""Tests for metadata tags: precedence (Config < ambient < per-call), caps, the
ambient context manager, and wire serialization (present when non-empty, omitted
when empty)."""

from __future__ import annotations

import unittest

import modelstat
from modelstat import Client, Config, FakeTransport, LlmCall
from modelstat.capture import build_batch
from modelstat.wire import cap_metadata


def cfg() -> Config:
    return Config("msk_test", "raw_sdk_openai", "test-app").with_device_id("dev_test")


class TestMetadataPrecedence(unittest.TestCase):
    def test_no_metadata_omits_the_wire_key(self) -> None:
        batch, _ = build_batch(cfg(), [LlmCall("openai", "sess_1")], 0)
        self.assertEqual(batch.events[0].metadata, {})
        self.assertNotIn("metadata", batch.events[0].to_dict())

    def test_config_defaults_under_per_call(self) -> None:
        c = cfg()
        c.metadata = {"environment": "prod", "feature": "default_feature"}
        call = LlmCall("openai", "sess_1").with_metadata(
            {"feature": "search", "team": "growth"}
        )
        batch, _ = build_batch(c, [call], 0)
        md = batch.events[0].metadata
        self.assertEqual(md["environment"], "prod")  # default-only survives
        self.assertEqual(md["feature"], "search")  # per-call overrides default
        self.assertEqual(md["team"], "growth")  # per-call-only added
        # Serializes as a flat object.
        self.assertEqual(batch.events[0].to_dict()["metadata"]["feature"], "search")

    def test_metadata_kwarg_sets_per_call_tags(self) -> None:
        call = LlmCall("openai", "sess_1", metadata={"feature": "x"})
        batch, _ = build_batch(cfg(), [call], 0)
        self.assertEqual(batch.events[0].metadata, {"feature": "x"})

    def test_config_under_ambient_under_per_call(self) -> None:
        # The ambient layer is captured on the hot path at record() time, so this
        # exercises the real Client/worker rather than build_batch directly.
        c = cfg()
        c.metadata = {"a": "config", "b": "config", "c": "config"}
        fake = FakeTransport()
        ms = Client.with_transport(c, fake)
        with modelstat.metadata({"b": "ambient", "c": "ambient"}):
            ms.record(LlmCall("openai", "sess_1").with_metadata({"c": "percall"}))
        ms.flush()
        md = fake.batches()[0]["events"][0]["metadata"]
        self.assertEqual(md["a"], "config")  # config-only
        self.assertEqual(md["b"], "ambient")  # ambient overrides config
        self.assertEqual(md["c"], "percall")  # per-call overrides ambient+config
        ms.shutdown()

    def test_ambient_scope_resets_on_exit_and_exception(self) -> None:
        fake = FakeTransport()
        ms = Client.with_transport(cfg(), fake)
        with modelstat.metadata({"scoped": "yes"}):
            ms.record(LlmCall("openai", "inside"))
        ms.record(LlmCall("openai", "outside"))
        # A raising block still resets the ambient layer.
        with self.assertRaises(ValueError):
            with modelstat.metadata({"scoped": "boom"}):
                raise ValueError("boom")
        ms.record(LlmCall("openai", "after"))
        ms.flush()

        events = [e for b in fake.batches() for e in b["events"]]
        by_sess = {e["session_id"]: e for e in events}
        self.assertEqual(by_sess["inside"]["metadata"]["scoped"], "yes")
        self.assertNotIn("metadata", by_sess["outside"])
        self.assertNotIn("metadata", by_sess["after"])
        ms.shutdown()

    def test_nested_ambient_blocks_merge(self) -> None:
        fake = FakeTransport()
        ms = Client.with_transport(cfg(), fake)
        with modelstat.metadata({"outer": "1", "shared": "outer"}):
            with modelstat.metadata({"inner": "2", "shared": "inner"}):
                ms.record(LlmCall("openai", "sess_1"))
        ms.flush()
        md = fake.batches()[0]["events"][0]["metadata"]
        self.assertEqual(md["outer"], "1")
        self.assertEqual(md["inner"], "2")
        self.assertEqual(md["shared"], "inner")  # inner wins
        ms.shutdown()


class TestMetadataCaps(unittest.TestCase):
    def test_excess_keys_dropped_by_sorted_order(self) -> None:
        c = cfg()
        # 20 keys k00..k19 -> only the 16 smallest survive (k00..k15).
        c.metadata = {f"k{i:02d}": "v" for i in range(20)}
        batch, _ = build_batch(c, [LlmCall("openai", "sess_1")], 0)
        md = batch.events[0].metadata
        self.assertEqual(len(md), 16)
        self.assertIn("k15", md)
        self.assertNotIn("k16", md)

    def test_over_long_key_and_value_truncated(self) -> None:
        out = cap_metadata({"k" * 100: "v" * 500})
        (key, value), = out.items()
        self.assertEqual(len(key), 64)
        self.assertEqual(len(value), 256)
        self.assertFalse(value.endswith("…"))

    def test_value_truncation_via_build_batch(self) -> None:
        call = LlmCall("openai", "sess_1", metadata={"big": "v" * 500})
        batch, _ = build_batch(cfg(), [call], 0)
        self.assertEqual(len(batch.events[0].metadata["big"]), 256)


class TestMetadataSerialization(unittest.TestCase):
    def test_present_when_non_empty_omitted_when_empty(self) -> None:
        present = LlmCall("openai", "s", metadata={"feature": "x"})
        batch, _ = build_batch(cfg(), [present], 0)
        self.assertEqual(batch.events[0].to_dict()["metadata"], {"feature": "x"})

        empty, _ = build_batch(cfg(), [LlmCall("openai", "s2")], 0)
        self.assertNotIn("metadata", empty.events[0].to_dict())


if __name__ == "__main__":
    unittest.main()
