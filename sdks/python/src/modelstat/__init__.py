"""modelstat -- a privacy-first SDK for wrapping the LLM calls your backend
already makes and shipping **redacted** usage to modelstat, without adding
latency to live requests.

The hot path (:meth:`Client.record`) does nothing but copy your already-in-hand
call into a bounded buffer and return. A background worker thread redacts,
batches, and ships off the request path. On overflow the newest record is
dropped and a counter increments -- your request is never blocked and never
grows memory unbounded.

Modes
-----
* **Local daemon (default).** Hand calls to a local modelstat daemon over
  loopback; it summarizes with a local Qwen model and ships only redacted
  abstracts. Raw text never leaves the machine.
* **Remote.** Ship directly to the modelstat server (no local model). With
  ``raw=True``, send full floor-redacted turns for server-side summarization.

Example
-------
.. code-block:: python

    from modelstat import Client, Config, LlmCall, TokenUsage

    # Org-scoped ingest key binds traffic to your account; remote mode here.
    cfg = Config("msk_live_...", "raw_sdk_openai").with_remote(
        "https://api.modelstat.ai", raw=True
    )

    with Client(cfg) as ms:  # shutdown() flushes on the way out
        # ... after your real LLM call returns ...
        ms.record(
            LlmCall("openai", "session-or-trace-id")
            .model_("gpt-x")
            .with_tokens(TokenUsage(input=800, output=120))
            .text("the prompt", "the completion")
        )
"""

from __future__ import annotations

from ._version import __version__
from .capture import LlmCall, ToolCallInput, build_batch
from .client import Client
from .config import DEFAULT_DAEMON_URL, Config, Mode, RedactionPolicy
from .redact import Redacted, redact
from .transport import FakeTransport, HttpTransport, Transport, TransportError
from .wire import (
    BillingMode,
    EventKind,
    GitContext,
    IngestBatch,
    RawEvent,
    TokenUsage,
    ToolCallStatus,
    ToolCallWire,
    batch_id,
    content_hash,
    source_event_id,
)

__all__ = [
    "__version__",
    # client + config
    "Client",
    "Config",
    "Mode",
    "RedactionPolicy",
    "DEFAULT_DAEMON_URL",
    # capture
    "LlmCall",
    "ToolCallInput",
    "build_batch",
    # redaction
    "redact",
    "Redacted",
    # transports
    "Transport",
    "HttpTransport",
    "FakeTransport",
    "TransportError",
    # wire
    "IngestBatch",
    "RawEvent",
    "ToolCallWire",
    "TokenUsage",
    "GitContext",
    "EventKind",
    "BillingMode",
    "ToolCallStatus",
    "content_hash",
    "source_event_id",
    "batch_id",
]
