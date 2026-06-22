"""The ingest wire contract, as a **self-contained** set of dataclasses.

This package is Apache-2.0 and must not depend on the (BSL-licensed) server
``modelstat-core``, so the shapes that cross ``POST /v1/ingest`` are re-declared
here. They mirror ``modelstat-core``'s ``RawEvent`` / ``ToolCallWire`` /
``IngestBatch`` field-for-field; the golden-vector tests pin the deterministic
id derivation to the server's algorithm so the two can never silently drift.
Ids ride the wire as plain strings (the server deserializes them into its typed
newtypes).

PRIVACY INVARIANT (mirrors the server contract): tool-call records carry only
hashes, byte sizes, and allowlisted command verbs -- never raw args, results,
paths, or command text.

Serialization rules (must match the server EXACTLY):

* JSON keys are ``snake_case`` -- no renames.
* The producing client's version ships as ``daemon_version`` (NOT
  ``client_version``); the AI-tool label ships as ``agent`` (NOT ``tool``).
* Optional keys are *omitted* when absent -- we never emit an explicit ``null``,
  because the wire contract is additive and a stray ``null`` is not the same as
  an absent key.
* A missing or misnamed REQUIRED field is an HTTP 400 that rejects the whole
  batch, so every required field below is always present in the emitted dict.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from typing import Any, Dict, List, Optional

import blake3

__all__ = [
    "TokenUsage",
    "EventKind",
    "PricingMode",
    "ToolCallStatus",
    "GitContext",
    "RawEvent",
    "ToolCallWire",
    "IngestBatch",
    "content_hash",
    "source_event_id",
    "batch_id",
    "format_rfc3339",
    "cap_metadata",
    "METADATA_MAX_ENTRIES",
    "METADATA_MAX_KEY_CHARS",
    "METADATA_MAX_VALUE_CHARS",
]


# ---- metadata caps ----------------------------------------------------------

# Client-side caps for the per-call ``metadata`` map, enforced before a batch
# leaves the process so an over-large map can never trip an HTTP 400 (or bloat
# the wire). At most ``METADATA_MAX_ENTRIES`` entries survive (excess keys
# dropped deterministically in sorted-key order); each key is truncated to
# ``METADATA_MAX_KEY_CHARS`` and each value to ``METADATA_MAX_VALUE_CHARS``
# Unicode code points.
METADATA_MAX_ENTRIES = 16
METADATA_MAX_KEY_CHARS = 64
METADATA_MAX_VALUE_CHARS = 256


def cap_metadata(metadata: Dict[str, str]) -> Dict[str, str]:
    """Apply the metadata caps to a resolved map.

    Keep at most :data:`METADATA_MAX_ENTRIES` entries -- the
    lexicographically-smallest keys, so the drop is deterministic -- truncating
    each key to :data:`METADATA_MAX_KEY_CHARS` and each value to
    :data:`METADATA_MAX_VALUE_CHARS` code points (no elision marker; tags are
    identifiers, not prose). Returns a fresh dict with keys in sorted order.
    """
    out: Dict[str, str] = {}
    for key in sorted(metadata.keys())[:METADATA_MAX_ENTRIES]:
        capped_key = key[:METADATA_MAX_KEY_CHARS]
        out[capped_key] = str(metadata[key])[:METADATA_MAX_VALUE_CHARS]
    return out


# ---- RFC3339 timestamp formatting ------------------------------------------


def format_rfc3339(dt: datetime) -> str:
    """Format ``dt`` as an RFC3339 UTC string with millisecond precision.

    Produces e.g. ``"2026-06-19T00:00:00.000Z"`` -- the exact shape the server
    expects. Naive datetimes are assumed to be UTC; aware datetimes are
    converted to UTC. Millisecond (not microsecond) precision matches the
    ``source_ref`` derivation, which uses ``timestamp_millis``.
    """
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    dt = dt.astimezone(timezone.utc)
    millis = dt.microsecond // 1000
    return f"{dt.strftime('%Y-%m-%dT%H:%M:%S')}.{millis:03d}Z"


# ---- token usage ------------------------------------------------------------


@dataclass
class TokenUsage:
    """The five token classes (a fixed taxonomy). Counts default to zero.

    All five keys are always emitted (the server expects the object), so this
    serializes to ``{input, output, cache_creation, cache_read, reasoning}``
    even when every count is zero.
    """

    input: int = 0
    output: int = 0
    cache_creation: int = 0
    cache_read: int = 0
    reasoning: int = 0

    def total(self) -> int:
        """Sum across all five classes."""
        return (
            self.input
            + self.output
            + self.cache_creation
            + self.cache_read
            + self.reasoning
        )

    def to_dict(self) -> Dict[str, int]:
        return {
            "input": self.input,
            "output": self.output,
            "cache_creation": self.cache_creation,
            "cache_read": self.cache_read,
            "reasoning": self.reasoning,
        }


# ---- enums (serialize to snake_case wire strings) ---------------------------


class EventKind(str, Enum):
    """The structural kind of a source event."""

    USER_MESSAGE = "user_message"
    ASSISTANT_MESSAGE = "assistant_message"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    SUMMARY = "summary"


class PricingMode(str, Enum):
    """How the provider billed the call."""

    SUBSCRIPTION = "subscription"
    API = "api"


class ToolCallStatus(str, Enum):
    """Outcome of a tool invocation."""

    SUCCESS = "success"
    ERROR = "error"
    DENIED = "denied"
    TIMEOUT = "timeout"
    UNKNOWN = "unknown"


# ---- git context ------------------------------------------------------------


@dataclass
class GitContext:
    """Git context captured at the moment of the call (all optional)."""

    remote_slug: Optional[str] = None
    host: Optional[str] = None
    branch: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {}
        if self.remote_slug is not None:
            out["remote_slug"] = self.remote_slug
        if self.host is not None:
            out["host"] = self.host
        if self.branch is not None:
            out["branch"] = self.branch
        return out


# ---- wire records -----------------------------------------------------------


@dataclass
class RawEvent:
    """One LLM call as it crosses the ingest boundary.

    Small and numeric, with at most a short redacted excerpt of text. The wire
    key for the AI-tool label is ``agent`` (never ``tool``).
    """

    source_event_id: str
    ts: datetime
    kind: EventKind
    # The **agent** -- which AI tool/integration produced the call (e.g.
    # ``raw_sdk_openai``), not the provider. (The wire key is ``agent``.)
    agent: str
    provider: str
    session_id: str
    tokens: TokenUsage = field(default_factory=TokenUsage)
    model: Optional[str] = None
    cwd: Optional[str] = None
    git: Optional[GitContext] = None
    duration_ms: Optional[int] = None
    pricing_mode: Optional[PricingMode] = None
    # Redacted excerpt used to build summaries downstream. Capped at 320 chars
    # in the standard (floor-redacted) path; carries the full redacted turns in
    # remote-raw mode, where the server summarizes.
    content_excerpt: Optional[str] = None
    # Free-form attribution tags (``feature``, ``customer_id``, ``team``, ...),
    # merged from Config defaults, the ambient context layer, and per-call
    # values, then capped. Emitted only when non-empty.
    metadata: Dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {
            "source_event_id": self.source_event_id,
            "ts": format_rfc3339(self.ts),
            "kind": self.kind.value,
            "agent": self.agent,
            "provider": self.provider,
            "session_id": self.session_id,
            "tokens": self.tokens.to_dict(),
        }
        # Optional keys -- omit when absent (never emit null).
        if self.model is not None:
            out["model"] = self.model
        if self.cwd is not None:
            out["cwd"] = self.cwd
        if self.git is not None:
            out["git"] = self.git.to_dict()
        if self.duration_ms is not None:
            out["duration_ms"] = self.duration_ms
        if self.pricing_mode is not None:
            out["pricing_mode"] = self.pricing_mode.value
        if self.content_excerpt is not None:
            out["content_excerpt"] = self.content_excerpt
        # Emit ``metadata`` only when non-empty (never send an empty object).
        if self.metadata:
            out["metadata"] = self.metadata
        return out


@dataclass
class ToolCallWire:
    """One tool invocation, privacy-reduced. Hashes and sizes only."""

    external_call_id: str
    session_id: str
    source_event_id: str
    # The **agent** (AI tool) that ran the call -- same space as RawEvent.agent.
    agent: str
    # ``builtin`` or ``mcp:<server>``.
    server: str
    # Bare tool name (``Bash``, ``create_pr``).
    name: str
    call_index: int
    started_at: datetime
    status: ToolCallStatus
    # Hex sha256 of the serialized input; ``""`` when the call had no input.
    args_hash: str
    # Sha256 of the sorted top-level arg key names joined by ``,``; the literal
    # ``none`` when the input is not an object.
    signature_hash: str
    args_bytes: int
    result_bytes: int
    segment_id: Optional[str] = None
    turn_index: Optional[int] = None
    ended_at: Optional[datetime] = None
    model: Optional[str] = None
    command_families: List[str] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {
            "external_call_id": self.external_call_id,
            "session_id": self.session_id,
            "source_event_id": self.source_event_id,
            "agent": self.agent,
            "server": self.server,
            "name": self.name,
            "call_index": self.call_index,
            "started_at": format_rfc3339(self.started_at),
            "status": self.status.value,
            "args_hash": self.args_hash,
            "signature_hash": self.signature_hash,
            "args_bytes": self.args_bytes,
            "result_bytes": self.result_bytes,
        }
        # ``segment_id`` and ``turn_index`` are intentionally never emitted by
        # the SDK (segmentation is produced downstream), but we honor them if
        # set for forward-compatibility.
        if self.segment_id is not None:
            out["segment_id"] = self.segment_id
        if self.turn_index is not None:
            out["turn_index"] = self.turn_index
        if self.ended_at is not None:
            out["ended_at"] = format_rfc3339(self.ended_at)
        if self.model is not None:
            out["model"] = self.model
        # Omit ``command_families`` when empty; the server caps it at 3.
        if self.command_families:
            out["command_families"] = self.command_families
        return out


@dataclass
class IngestBatch:
    """The full ingest payload.

    The SDK only ever emits ``events`` (+ ``tool_calls``); segmentation,
    summarization, titles, and session-installs are produced downstream by the
    daemon or server.
    """

    batch_id: str
    device_id: str
    # This SDK build's version string (<=40 chars). Ships as the wire
    # ``daemon_version`` field -- the server's name for the producing client's
    # version; an SDK is just another producer of the ingest contract.
    daemon_version: str
    events: List[RawEvent] = field(default_factory=list)
    tool_calls: List[ToolCallWire] = field(default_factory=list)
    # Per-batch taxonomy auto-detection toggle. ``None`` = server default
    # (taxonomy auto/on); ``False`` = skip taxonomy auto-detection for this
    # batch; ``True`` = force it on. SDK/backend integrations default this to
    # ``False`` (backend LLM usage isn't interactive work-sessions). Included in
    # ``to_dict()`` only when not None.
    auto_taxonomy: Optional[bool] = None

    def to_dict(self) -> Dict[str, Any]:
        out: Dict[str, Any] = {
            "batch_id": self.batch_id,
            "device_id": self.device_id,
            "daemon_version": self.daemon_version,
            "events": [e.to_dict() for e in self.events],
        }
        # Omit ``tool_calls`` entirely when empty (do NOT send an empty list).
        if self.tool_calls:
            out["tool_calls"] = [t.to_dict() for t in self.tool_calls]
        # Optional key -- omit when None (never emit null).
        if self.auto_taxonomy is not None:
            out["auto_taxonomy"] = self.auto_taxonomy
        return out


# ---- deterministic ids (mirror modelstat-core::ids) -------------------------

# The ASCII unit separator joined between consecutive parts (never before the
# first or after the last). This exact framing is what makes ``["ab", ""]``
# differ from ``["a", "b"]``.
_UNIT_SEPARATOR = b"\x1f"


def content_hash(parts: List[str]) -> str:
    """blake3 content hash of ``parts``.

    The parts' UTF-8 bytes are joined by a single ``0x1F`` byte between
    consecutive parts (NOT before the first / after the last), then hashed with
    blake3 and rendered as lowercase hex truncated to the first 32 characters.
    Identical to the server's ``content_hash`` so client- and server-derived ids
    agree.
    """
    joined = _UNIT_SEPARATOR.join(p.encode("utf-8") for p in parts)
    return blake3.blake3(joined).hexdigest()[:32]


def source_event_id(device_id: str, source_ref: str) -> str:
    """Stable per-source-event dedupe key: ``evt_<content_hash(device, ref)>``.

    ``source_ref`` must be stable for the same logical call across retries.
    """
    return "evt_" + content_hash([device_id, source_ref])


def batch_id(source_event_ids: List[str]) -> str:
    """Deterministic batch id over the (sorted) source-event ids it carries.

    A resend of the same events reuses the id and the server's manifest dedupes
    it.
    """
    return "batch_" + content_hash(sorted(source_event_ids))
