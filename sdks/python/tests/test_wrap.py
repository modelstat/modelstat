"""Tests for modelstat.wrap(): wrapping a fake OpenAI-shaped and Anthropic-shaped
client auto-records exactly one call with the right provider/model/tokens, returns
the underlying response untouched, and a record failure doesn't break the call."""

from __future__ import annotations

import time
import unittest
from datetime import datetime, timedelta, timezone

import modelstat
from modelstat import Client, Config, FakeTransport


def cfg(agent: str = "raw_sdk_openai") -> Config:
    return Config("msk_test", agent).with_device_id("dev_test")


# ---- fake provider clients --------------------------------------------------


class _FakeOpenAI:
    """Stand-in for ``OpenAI()`` -- ``chat.completions.create`` + an unrelated
    method to prove pass-through."""

    def __init__(self, response: object, calls: list) -> None:
        self.calls = calls
        self.models = _Models()
        self.chat = _Chat(response, calls)


class _Models:
    def list(self) -> str:
        return "models.list"


class _Chat:
    def __init__(self, response: object, calls: list) -> None:
        self.completions = _Completions(response, calls)


class _Completions:
    def __init__(self, response: object, calls: list) -> None:
        self._response = response
        self._calls = calls

    def create(self, **kwargs: object) -> object:
        self._calls.append(kwargs)
        return self._response


class _FakeAnthropic:
    """Stand-in for ``Anthropic()`` -- ``messages.create``."""

    def __init__(self, response: object, calls: list) -> None:
        self.calls = calls
        self.messages = _Messages(response, calls)


class _Messages:
    def __init__(self, response: object, calls: list) -> None:
        self._response = response
        self._calls = calls

    def create(self, **kwargs: object) -> object:
        self._calls.append(kwargs)
        return self._response


class TestWrap(unittest.TestCase):
    def test_openai_autorecords_with_provider_model_tokens(self) -> None:
        calls: list = []
        response = {
            "model": "gpt-x",
            "usage": {"prompt_tokens": 800, "completion_tokens": 120},
            "choices": [{"message": {"role": "assistant", "content": "the completion"}}],
        }
        fake = FakeTransport()
        ms = Client.with_transport(cfg("raw_sdk_openai"), fake)
        client = modelstat.wrap(_FakeOpenAI(response, calls), recorder=ms)

        out = client.chat.completions.create(
            model="gpt-x", messages=[{"role": "user", "content": "the prompt"}]
        )
        self.assertIs(out, response)  # response returned untouched
        self.assertEqual(len(calls), 1)  # real call ran exactly once

        ms.flush()
        events = [e for b in fake.batches() for e in b["events"]]
        self.assertEqual(len(events), 1)
        ev = events[0]
        self.assertEqual(ev["provider"], "openai")
        self.assertEqual(ev["model"], "gpt-x")
        self.assertEqual(ev["tokens"]["input"], 800)
        self.assertEqual(ev["tokens"]["output"], 120)
        self.assertIn("the prompt", ev["content_excerpt"])
        self.assertIn("the completion", ev["content_excerpt"])
        ms.shutdown()

    def test_a_wrapped_call_is_dated_when_the_request_went_out(self) -> None:
        """Recording happens after the response resolves, so a call built there
        would carry the LATER instant. The interceptor reads the clock before it
        forwards, and that is what ``ts`` / ``started_at`` carry."""

        class _SlowCompletions:
            def create(self, **kwargs: object) -> object:
                time.sleep(0.025)
                return {"model": "gpt-x", "usage": {"prompt_tokens": 1}}

        class _SlowChat:
            def __init__(self) -> None:
                self.completions = _SlowCompletions()

        class _SlowOpenAI:
            def __init__(self) -> None:
                self.chat = _SlowChat()

        fake = FakeTransport()
        ms = Client.with_transport(cfg(), fake)
        client = modelstat.wrap(_SlowOpenAI(), recorder=ms)

        before = datetime.now(timezone.utc)
        client.chat.completions.create(model="gpt-x", messages=[])
        after = datetime.now(timezone.utc)
        ms.flush()

        ev = [e for b in fake.batches() for e in b["events"]][0]
        started = datetime.fromisoformat(ev["started_at"].replace("Z", "+00:00"))
        self.assertEqual(ev["ts"], ev["started_at"], "ts carries the same instant")
        # The wire carries milliseconds, so the reading is the reference instant
        # rounded DOWN -- compare against the same resolution, not below it.
        self.assertGreaterEqual(started, before - timedelta(milliseconds=1))
        self.assertLess(
            started,
            after - timedelta(milliseconds=20),
            "the response instant leaked in place of the request instant",
        )
        # One whole response, never a first chunk -- so no instant is invented.
        self.assertNotIn("first_token_at", ev)
        ms.shutdown()

    def test_anthropic_autorecords_with_anthropic_token_shape(self) -> None:
        calls: list = []
        response = {
            "model": "claude-x",
            "usage": {"input_tokens": 1200, "output_tokens": 300},
            "content": [{"type": "text", "text": "hi there"}],
        }
        fake = FakeTransport()
        ms = Client.with_transport(cfg("raw_sdk_anthropic"), fake)
        client = modelstat.wrap(_FakeAnthropic(response, calls), recorder=ms)

        out = client.messages.create(
            model="claude-x",
            system="be terse",
            messages=[{"role": "user", "content": "hello"}],
        )
        self.assertIs(out, response)
        self.assertEqual(len(calls), 1)

        ms.flush()
        events = [e for b in fake.batches() for e in b["events"]]
        self.assertEqual(len(events), 1)
        ev = events[0]
        self.assertEqual(ev["provider"], "anthropic")
        self.assertEqual(ev["model"], "claude-x")
        self.assertEqual(ev["tokens"]["input"], 1200)
        self.assertEqual(ev["tokens"]["output"], 300)
        ms.shutdown()

    def test_passthrough_unrelated_methods(self) -> None:
        ms = Client.with_transport(cfg(), FakeTransport())
        client = modelstat.wrap(_FakeOpenAI({}, []), recorder=ms)
        self.assertEqual(client.models.list(), "models.list")
        ms.shutdown()

    def test_record_failure_never_breaks_the_call(self) -> None:
        calls: list = []
        response = {"model": "gpt-x", "usage": {}, "choices": []}
        ms = Client.with_transport(cfg(), FakeTransport())

        def boom(_call: object) -> None:
            raise RuntimeError("record boom")

        ms.record = boom  # type: ignore[method-assign]
        client = modelstat.wrap(_FakeOpenAI(response, calls), recorder=ms)
        out = client.chat.completions.create(model="gpt-x", messages=[])
        self.assertIs(out, response)
        self.assertEqual(len(calls), 1)
        ms.shutdown()

    def test_wrap_default_metadata_rides_recorded_call(self) -> None:
        calls: list = []
        response = {
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            "choices": [{"message": {"content": "ok"}}],
        }
        c = cfg()
        c.metadata = {"environment": "prod"}
        fake = FakeTransport()
        ms = Client.with_transport(c, fake)
        client = modelstat.wrap(
            _FakeOpenAI(response, calls), recorder=ms, metadata={"feature": "search"}
        )
        client.chat.completions.create(model="gpt-x", messages=[])
        ms.flush()
        md = fake.batches()[0]["events"][0]["metadata"]
        self.assertEqual(md["environment"], "prod")  # Config default
        self.assertEqual(md["feature"], "search")  # wrap-default (per-call layer)
        ms.shutdown()

    def test_unknown_client_raises(self) -> None:
        ms = Client.with_transport(cfg(), FakeTransport())
        with self.assertRaises(TypeError):
            modelstat.wrap(object(), recorder=ms)
        ms.shutdown()

    def test_missing_recorder_raises(self) -> None:
        with self.assertRaises(TypeError):
            modelstat.wrap(_FakeOpenAI({}, []))


if __name__ == "__main__":
    unittest.main()
