"""Tests for the wire contract: deterministic ids, the golden vector, and the
serialized JSON shape. Mirrors the Rust ``wire.rs`` test module."""

from __future__ import annotations

import unittest
from datetime import datetime, timezone

from modelstat.wire import (
    EventKind,
    RawEvent,
    TokenUsage,
    batch_id,
    content_hash,
    format_rfc3339,
    source_event_id,
)


class TestIds(unittest.TestCase):
    def test_source_event_id_is_deterministic_and_prefixed(self) -> None:
        a = source_event_id("dev_1", "sess::100::1")
        b = source_event_id("dev_1", "sess::100::1")
        self.assertEqual(a, b)
        self.assertNotEqual(a, source_event_id("dev_1", "sess::100::2"))
        self.assertTrue(a.startswith("evt_"))
        self.assertEqual(len(a), len("evt_") + 32)

    def test_batch_id_is_order_independent(self) -> None:
        ids1 = ["evt_a", "evt_b"]
        ids2 = ["evt_b", "evt_a"]
        self.assertEqual(batch_id(ids1), batch_id(ids2))
        self.assertTrue(batch_id(ids1).startswith("batch_"))

    def test_content_hash_golden_vector(self) -> None:
        # 32-char lowercase-hex output, deterministic, separator-sensitive.
        self.assertEqual(len(content_hash(["a", "b"])), 32)
        self.assertEqual(content_hash(["a", "b"]), content_hash(["a", "b"]))
        # "a"\x1f"b" must differ from "ab"\x1f"" -- the unit-separator framing.
        self.assertNotEqual(content_hash(["a", "b"]), content_hash(["ab", ""]))
        # Lowercase hex only.
        self.assertEqual(
            content_hash(["a", "b"]), content_hash(["a", "b"]).lower()
        )


class TestSerialization(unittest.TestCase):
    def test_event_serializes_to_expected_shape(self) -> None:
        ev = RawEvent(
            source_event_id="evt_x",
            ts=datetime(2026, 6, 19, 0, 0, 0, tzinfo=timezone.utc),
            kind=EventKind.ASSISTANT_MESSAGE,
            agent="raw_sdk_openai",
            provider="openai",
            session_id="sess_1",
            tokens=TokenUsage(input=10, output=5),
            model="gpt-x",
            duration_ms=1200,
            content_excerpt="hello",
        )
        j = ev.to_dict()
        self.assertEqual(j["kind"], "assistant_message")
        self.assertEqual(j["agent"], "raw_sdk_openai")
        self.assertEqual(j["tokens"]["input"], 10)
        # Tokens object always carries all five classes.
        self.assertEqual(
            set(j["tokens"].keys()),
            {"input", "output", "cache_creation", "cache_read", "reasoning"},
        )
        # Absent optionals must not serialize (additive wire contract).
        self.assertNotIn("cwd", j)
        self.assertNotIn("git", j)

    def test_rfc3339_millisecond_utc_shape(self) -> None:
        ts = datetime(2026, 6, 19, 0, 0, 0, 0, tzinfo=timezone.utc)
        self.assertEqual(format_rfc3339(ts), "2026-06-19T00:00:00.000Z")
        ts2 = datetime(2026, 6, 19, 12, 34, 56, 789000, tzinfo=timezone.utc)
        self.assertEqual(format_rfc3339(ts2), "2026-06-19T12:34:56.789Z")
        # Naive datetimes are assumed UTC.
        naive = datetime(2026, 6, 19, 0, 0, 0)
        self.assertEqual(format_rfc3339(naive), "2026-06-19T00:00:00.000Z")


if __name__ == "__main__":
    unittest.main()
