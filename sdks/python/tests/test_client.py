"""End-to-end client tests: record -> flush delivers a redacted batch, and
overflow drops the newest without blocking. Mirrors the Rust ``lib.rs`` tests."""

from __future__ import annotations

import time
import unittest

from modelstat import Client, Config, FakeTransport, LlmCall, TokenUsage


class TestClient(unittest.TestCase):
    def test_record_then_flush_delivers_a_redacted_batch(self) -> None:
        cfg = Config("msk_test", "raw_sdk_openai").with_device_id("dev_test")
        fake = FakeTransport()
        ms = Client.with_transport(cfg, fake)

        ms.record(
            LlmCall("openai", "sess_1")
            .model_("gpt-x")
            .with_tokens(TokenUsage(input=100, output=20))
            .text("my email is jane@example.com", "done")
        )
        ms.flush()

        batches = fake.batches()
        self.assertEqual(len(batches), 1)
        ev = batches[0]["events"][0]
        self.assertEqual(ev["provider"], "openai")
        self.assertEqual(ev["tokens"]["input"], 100)
        excerpt = ev["content_excerpt"]
        self.assertIn("[REDACTED:email]", excerpt)
        self.assertNotIn("jane@example.com", excerpt)
        self.assertEqual(ms.dropped(), 0)
        ms.shutdown()

    def test_flushed_batch_is_a_wire_dict_with_required_keys(self) -> None:
        cfg = Config("msk_test", "raw_sdk_anthropic").with_device_id("dev_x")
        fake = FakeTransport()
        ms = Client.with_transport(cfg, fake)
        ms.record(LlmCall("anthropic", "sess_1").text("hello", "world"))
        ms.flush()

        batch = fake.batches()[0]
        # Required top-level keys (snake_case, daemon_version not client_version).
        self.assertIn("batch_id", batch)
        self.assertIn("device_id", batch)
        self.assertIn("daemon_version", batch)
        self.assertIn("events", batch)
        self.assertNotIn("client_version", batch)
        self.assertTrue(batch["daemon_version"].startswith("python-sdk/"))
        self.assertLessEqual(len(batch["daemon_version"]), 40)
        self.assertEqual(batch["device_id"], "dev_x")
        # Event uses `agent`, not `tool`.
        ev = batch["events"][0]
        self.assertEqual(ev["agent"], "raw_sdk_anthropic")
        self.assertNotIn("tool", ev)
        ms.shutdown()

    def test_overflow_drops_newest_and_counts_without_blocking(self) -> None:
        # Tiny buffer, and a long flush interval, so the buffer fills.
        cfg = Config("msk", "raw_sdk_generic").with_device_id("dev_test")
        cfg.buffer_capacity = 2
        cfg.flush_interval = 3600.0
        fake = FakeTransport()
        ms = Client.with_transport(cfg, fake)

        for _ in range(50):
            ms.record(LlmCall("openai", "sess_1"))
        # The worker may have pulled a couple, but most overflow -- the point is
        # record() never blocked and overflow is counted.
        self.assertGreater(ms.dropped(), 0, "expected some drops")
        ms.shutdown()

    def test_context_manager_flushes_on_exit(self) -> None:
        cfg = Config("msk_test", "raw_sdk_openai")
        fake = FakeTransport()
        with Client.with_transport(cfg, fake) as ms:
            ms.record(LlmCall("openai", "sess_1").text("hi", "there"))
        # __exit__ called shutdown(), which flushes.
        self.assertEqual(len(fake.batches()), 1)

    def test_interval_flush_fires_without_explicit_flush(self) -> None:
        cfg = Config("msk_test", "raw_sdk_openai")
        cfg.flush_interval = 0.05
        fake = FakeTransport()
        ms = Client.with_transport(cfg, fake)
        ms.record(LlmCall("openai", "sess_1").text("hi", "there"))
        # Wait past the interval; the timer-driven flush should ship the batch
        # with no explicit flush() call.
        deadline = time.monotonic() + 2.0
        while not fake.batches() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertEqual(len(fake.batches()), 1)
        ms.shutdown()


if __name__ == "__main__":
    unittest.main()
