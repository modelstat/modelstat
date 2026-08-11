"""Tests for the capture -> wire conversion. Mirrors the Rust ``capture.rs``
test module."""

from __future__ import annotations

import hashlib
import json
import unittest
from datetime import timedelta

from modelstat.capture import EXCERPT_MAX_CHARS, LlmCall, ToolCallInput, build_batch
from modelstat.config import Config
from modelstat.wire import ToolCallStatus


def cfg() -> Config:
    return Config("msk_test", "raw_sdk_openai").with_device_id("dev_test")


class TestBuildBatch(unittest.TestCase):
    def test_build_batch_redacts_excerpt_and_caps_length(self) -> None:
        call = LlmCall("openai", "sess_1").model_("gpt-x").text(
            "here is my key sk-ant-0123456789abcdefghijABCDEF",
            "ok done",
        )
        batch, _seq = build_batch(cfg(), [call], 0)

        self.assertEqual(len(batch.events), 1)
        ev = batch.events[0]
        self.assertEqual(ev.agent, "raw_sdk_openai")
        self.assertEqual(ev.provider, "openai")
        self.assertIsNotNone(ev.content_excerpt)
        excerpt = ev.content_excerpt
        assert excerpt is not None
        self.assertIn("[REDACTED:anthropic_key]", excerpt)
        self.assertNotIn("sk-ant-0123", excerpt)
        self.assertLessEqual(len(excerpt), EXCERPT_MAX_CHARS + 1)
        self.assertTrue(ev.source_event_id.startswith("evt_"))
        self.assertTrue(batch.batch_id.startswith("batch_"))

    def test_excerpt_truncates_to_320_plus_marker(self) -> None:
        long_prompt = "x" * 1000
        call = LlmCall("openai", "sess_1").text(long_prompt, "")
        batch, _ = build_batch(cfg(), [call], 0)
        excerpt = batch.events[0].content_excerpt
        assert excerpt is not None
        # 320 code points + the single-character elision marker.
        self.assertEqual(len(excerpt), EXCERPT_MAX_CHARS + 1)
        self.assertTrue(excerpt.endswith("…"))

    def test_empty_text_yields_no_excerpt_key(self) -> None:
        call = LlmCall("openai", "sess_1")  # no prompt/completion
        batch, _ = build_batch(cfg(), [call], 0)
        self.assertIsNone(batch.events[0].content_excerpt)
        self.assertNotIn("content_excerpt", batch.events[0].to_dict())

    def test_tool_calls_carry_hashes_not_raw_args(self) -> None:
        args = {"command": "rm -rf /tmp/secret", "timeout": 5}
        call = LlmCall("anthropic", "sess_1")
        call.tool_calls.append(
            ToolCallInput(
                name="Bash",
                server="builtin",
                args=args,
                result_bytes=128,
                status=ToolCallStatus.SUCCESS,
                command_families=["rm"],
            )
        )
        batch, _ = build_batch(cfg(), [call], 0)

        self.assertEqual(len(batch.tool_calls), 1)
        tc = batch.tool_calls[0]
        self.assertEqual(tc.name, "Bash")
        expected_serialized = json.dumps(args, separators=(",", ":"), sort_keys=False)
        self.assertEqual(tc.args_bytes, len(expected_serialized.encode("utf-8")))
        self.assertEqual(len(tc.args_hash), 64)  # sha256 hex
        self.assertNotEqual(tc.signature_hash, "none")
        # signature == sha256 of sorted top-level keys joined by ",".
        self.assertEqual(
            tc.signature_hash,
            hashlib.sha256(b"command,timeout").hexdigest(),
        )
        self.assertTrue(tc.external_call_id.startswith("tc_"))
        self.assertEqual(len(tc.external_call_id), len("tc_") + 16)

        # The raw command must never appear anywhere in the serialized batch.
        serialized = json.dumps(batch.to_dict())
        self.assertNotIn("rm -rf /tmp/secret", serialized)

    def test_no_args_yields_none_signature_and_empty_hash(self) -> None:
        call = LlmCall("anthropic", "sess_1")
        call.tool_calls.append(
            ToolCallInput(name="Ping", status=ToolCallStatus.SUCCESS)
        )
        batch, _ = build_batch(cfg(), [call], 0)
        tc = batch.tool_calls[0]
        self.assertEqual(tc.args_hash, "")
        self.assertEqual(tc.signature_hash, "none")
        self.assertEqual(tc.args_bytes, 0)

    def test_command_families_capped_at_three(self) -> None:
        call = LlmCall("anthropic", "sess_1")
        call.tool_calls.append(
            ToolCallInput(
                name="Bash",
                status=ToolCallStatus.SUCCESS,
                command_families=["a", "b", "c", "d", "e"],
            )
        )
        batch, _ = build_batch(cfg(), [call], 0)
        self.assertEqual(batch.tool_calls[0].command_families, ["a", "b", "c"])

    def test_empty_tool_calls_key_omitted(self) -> None:
        call = LlmCall("openai", "sess_1").text("hi", "yo")
        batch, _ = build_batch(cfg(), [call], 0)
        self.assertNotIn("tool_calls", batch.to_dict())

    def test_auto_taxonomy_defaults_off_and_opts_in(self) -> None:
        # Default config: taxonomy off -> explicit ``auto_taxonomy: false``.
        batch, _ = build_batch(cfg(), [LlmCall("openai", "sess_1")], 0)
        self.assertEqual(batch.auto_taxonomy, False)
        self.assertEqual(batch.to_dict()["auto_taxonomy"], False)

        # Opt in: flag True -> wire ``auto_taxonomy: true``.
        on = cfg()
        on.auto_taxonomy = True
        batch, _ = build_batch(on, [LlmCall("openai", "sess_1")], 0)
        self.assertEqual(batch.auto_taxonomy, True)
        self.assertEqual(batch.to_dict()["auto_taxonomy"], True)

    def test_raw_mode_sends_full_untruncated_turns_still_floor_redacted(
        self,
    ) -> None:
        rcfg = (
            Config("msk", "raw_sdk_openai")
            .with_device_id("dev_test")
            .with_remote("https://api.modelstat.ai", raw=True)
        )
        long = "word " * 200  # > 320 chars
        call = LlmCall("openai", "sess_1").text(long, "AKIAIOSFODNN7EXAMPLE")
        batch, _ = build_batch(rcfg, [call], 0)
        excerpt = batch.events[0].content_excerpt
        assert excerpt is not None
        self.assertGreater(
            len(excerpt), EXCERPT_MAX_CHARS, "raw mode must not truncate"
        )
        self.assertIn(
            "[REDACTED:aws_access_key]",
            excerpt,
            "floor still applies in raw mode",
        )

    def test_seq_makes_distinct_ids_for_same_call(self) -> None:
        c1 = LlmCall("openai", "sess_1")
        c2 = LlmCall("openai", "sess_1")
        # Pin started_at so only seq differs.
        c2.started_at = c1.started_at
        batch, seq = build_batch(cfg(), [c1, c2], 0)
        self.assertEqual(seq, 2)
        self.assertNotEqual(
            batch.events[0].source_event_id,
            batch.events[1].source_event_id,
        )

    def test_states_the_instants_it_saw_and_omits_the_one_it_did_not(self) -> None:
        """The SDK is in the call path, so it can state the span's ends.

        ``started_at`` always ships (it is ``ts``'s own provenance made
        explicit); the first-token instant ships only when the caller watched a
        stream and said so, because a call that returns in one piece never had a
        first chunk to time.
        """
        quiet = LlmCall("openai", "sess_1")
        streamed = LlmCall("openai", "sess_2")
        streamed.started_at = quiet.started_at
        streamed.first_token_at = quiet.started_at + timedelta(milliseconds=140)

        batch, _ = build_batch(cfg(), [quiet, streamed], 0)
        a, b = batch.events[0], batch.events[1]

        self.assertEqual(a.started_at, quiet.started_at)
        self.assertEqual(a.ts, quiet.started_at, "ts is unchanged")
        self.assertIsNone(a.first_token_at, "no stream, no first chunk to time")
        self.assertEqual(b.first_token_at, streamed.first_token_at)

        # Additive on the wire: absent means absent, never null.
        self.assertNotIn("first_token_at", a.to_dict())
        self.assertIn("started_at", a.to_dict())


if __name__ == "__main__":
    unittest.main()
