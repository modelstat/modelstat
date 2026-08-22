"""The capture surface: what a caller hands the SDK per LLM call, and the
(worker-side) conversion into wire records.

Building an :class:`LlmCall` and calling :meth:`Client.record` is the only thing
that happens on the live request path -- it must stay a cheap move into a
buffer. All of the work here (redaction, hashing, id derivation) runs later, on
the background worker, off the hot path.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Dict, Iterable, List, Optional, Tuple

from . import wire
from .config import Config, RedactionPolicy
from .redact import redact
from .wire import (
    EventKind,
    GitContext,
    IngestBatch,
    RawEvent,
    TokenUsage,
    ToolCallStatus,
    ToolCallWire,
    cap_metadata,
)

__all__ = ["LlmCall", "ToolCallInput", "build_batch"]

# The excerpt cap for the standard (non-raw) path, in Unicode code points.
EXCERPT_MAX_CHARS = 320


def _now_utc() -> datetime:
    return datetime.now(timezone.utc)


@dataclass
class ToolCallInput:
    """One captured tool invocation.

    The SDK is in the call path, so it has the real args and result -- it
    hashes/sizes them here (never ships them raw).
    """

    # Bare tool name (``Bash``, ``create_pr``).
    name: str
    status: ToolCallStatus
    # ``builtin`` or ``mcp:<server>``.
    server: str = "builtin"
    # The call's arguments, if any. Hashed and sized; never shipped.
    args: Optional[Any] = None
    # Byte length of the result/output (the SDK sizes it; never ships it).
    result_bytes: int = 0
    started_at: datetime = field(default_factory=_now_utc)
    ended_at: Optional[datetime] = None
    # Allowlisted command verbs for shell-ish tools (<=3, each <=40 chars).
    command_families: List[str] = field(default_factory=list)


@dataclass
class LlmCall:
    """One captured LLM call.

    Construct directly with keyword arguments, or build incrementally with the
    chainable helpers (:meth:`model`, :meth:`with_tokens`, :meth:`text`).
    ``prompt`` / ``completion`` are raw here and are redacted on the worker.
    """

    provider: str
    # Trace/conversation id used to group calls into a session downstream.
    session_id: str
    model: Optional[str] = None
    kind: EventKind = EventKind.ASSISTANT_MESSAGE
    tokens: TokenUsage = field(default_factory=TokenUsage)
    started_at: datetime = field(default_factory=_now_utc)
    # When the first piece of the model's output arrived, if it was watched for
    # (a streamed response). Left ``None`` on a call that returns in one piece
    # -- there is no first chunk to have seen.
    first_token_at: Optional[datetime] = None
    duration_ms: Optional[int] = None
    prompt: Optional[str] = None
    completion: Optional[str] = None
    cwd: Optional[str] = None
    git: Optional[GitContext] = None
    tool_calls: List[ToolCallInput] = field(default_factory=list)
    # Per-call attribution tags. The highest-priority layer: these override the
    # ambient context layer and ``Config`` defaults on shared keys. Pass via the
    # ``metadata=`` keyword or merge with :meth:`with_metadata`. Capped before
    # send.
    metadata: Dict[str, str] = field(default_factory=dict)
    # Snapshot of the ambient (``with modelstat.metadata(...)``) tags captured on
    # the hot path at ``record()`` time -- *not* set by callers. The worker fills
    # this in because the merge runs later, off the ``with`` block. The middle
    # layer: above ``Config`` defaults, below :attr:`metadata`.
    ambient_metadata: Optional[Dict[str, str]] = None

    # ---- chainable builder helpers (ergonomic, mirror the Rust builder) -----

    def model_(self, model: str) -> "LlmCall":
        """Set the model. (Trailing underscore avoids shadowing the field.)"""
        self.model = model
        return self

    def with_tokens(self, tokens: TokenUsage) -> "LlmCall":
        """Set token usage."""
        self.tokens = tokens
        return self

    def text(self, prompt: str, completion: str) -> "LlmCall":
        """Set the prompt and completion text (raw; redacted on the worker)."""
        self.prompt = prompt
        self.completion = completion
        return self

    def with_metadata(self, tags: Dict[str, str]) -> "LlmCall":
        """Merge per-call attribution tags (each key overwrites any previous
        value, including a same-keyed default/ambient tag). Returns ``self`` for
        chaining."""
        self.metadata.update(tags)
        return self


def _truncate_chars(s: str, max_chars: int) -> str:
    """Truncate to at most ``max_chars`` Unicode code points, appending an
    elision marker. Python strings index by code point, so slicing is the direct
    equivalent of the Rust ``chars().take(max)``."""
    if len(s) <= max_chars:
        return s
    return s[:max_chars] + "…"


def _sha256_hex(data: bytes) -> str:
    """sha256 hex of ``data``."""
    return hashlib.sha256(data).hexdigest()


def _hash_args(args: Optional[Any]) -> Tuple[str, str, int]:
    """Build the privacy-reduced ``(args_hash, signature_hash, args_bytes)``
    triple for a tool call's arguments.

    Canonical JSON matches the Rust reference: compact separators and *insertion
    order preserved* (``sort_keys=False``) -- ``serde_json`` serializes a Map in
    its stored order, and Python's ``dict`` is insertion-ordered, so the byte
    sizes agree. ``signature_hash`` hashes the *sorted* top-level key names; it
    is the literal ``"none"`` when there are no args or the args are not a dict.
    """
    if args is None:
        return ("", "none", 0)
    serialized = json.dumps(args, separators=(",", ":"), sort_keys=False)
    serialized_bytes = serialized.encode("utf-8")
    args_hash = _sha256_hex(serialized_bytes)
    if isinstance(args, dict):
        keys = sorted(args.keys())
        signature = _sha256_hex(",".join(keys).encode("utf-8"))
    else:
        signature = "none"
    return (args_hash, signature, len(serialized_bytes))


def _build_excerpt(cfg: Config, call: LlmCall) -> Optional[str]:
    """Build the redacted excerpt from a call's prompt + completion, honoring
    the configured redaction policy and (for the standard path) the 320-char
    cap. Empty input yields ``None`` (the key is then omitted on the wire)."""
    joined = ""
    if call.prompt is not None:
        joined += call.prompt
    if call.completion is not None:
        if joined:
            joined += "\n---\n"
        joined += call.completion
    if not joined:
        return None

    if cfg.redaction == RedactionPolicy.FLOOR:
        scrubbed = redact(joined).text
    else:  # RedactionPolicy.NONE
        scrubbed = joined

    # Raw mode ships the full (redacted) turns for server-side summarization;
    # the standard path caps the excerpt.
    if cfg.sends_full_turns():
        return scrubbed
    return _truncate_chars(scrubbed, EXCERPT_MAX_CHARS)


def _resolve_metadata(cfg: Config, call: LlmCall) -> Dict[str, str]:
    """Resolve the per-event metadata: ``Config`` defaults are the base layer,
    the per-call ambient snapshot is the middle layer, and per-call tags are the
    top layer (each later layer wins on a shared key). The caps are then applied.
    Returns an empty dict when nothing is set (the wire key is then omitted)."""
    merged: Dict[str, str] = dict(cfg.metadata)
    if call.ambient_metadata:
        merged.update(call.ambient_metadata)
    if call.metadata:
        merged.update(call.metadata)
    if not merged:
        return {}
    return cap_metadata(merged)


def _event_from_call(
    cfg: Config, call: LlmCall, seq: int
) -> Tuple[RawEvent, List[ToolCallWire]]:
    """Convert one captured call into a wire event plus its tool-call records."""
    # Integer-millis since the epoch, matching Rust's ``timestamp_millis()``.
    # Computed with integer arithmetic (not ``ts * 1000``) to avoid float
    # rounding that could occasionally shift the floored millisecond and so
    # change the derived ``source_event_id``.
    ts = call.started_at
    started_millis = int(ts.timestamp()) * 1000 + ts.microsecond // 1000
    source_ref = f"{call.session_id}::{started_millis}::{seq}"
    src_event_id = wire.source_event_id(cfg.device_id, source_ref)

    event = RawEvent(
        source_event_id=src_event_id,
        ts=call.started_at,
        # The SDK held the clock for this call, so it states the span's ends
        # rather than leaving a reader to reconstruct them from ``ts`` and a
        # duration that may not be set.
        started_at=call.started_at,
        first_token_at=call.first_token_at,
        kind=call.kind,
        agent=cfg.agent,
        provider=call.provider,
        session_id=call.session_id,
        tokens=call.tokens,
        model=call.model,
        cwd=call.cwd,
        git=call.git,
        duration_ms=call.duration_ms,
        content_excerpt=_build_excerpt(cfg, call),
        metadata=_resolve_metadata(cfg, call),
    )

    tool_calls: List[ToolCallWire] = []
    for i, tc in enumerate(call.tool_calls):
        args_hash, signature_hash, args_bytes = _hash_args(tc.args)
        external_call_id = "tc_" + content_hash_tc(src_event_id, i)
        tool_calls.append(
            ToolCallWire(
                external_call_id=external_call_id,
                session_id=call.session_id,
                source_event_id=src_event_id,
                agent=cfg.agent,
                server=tc.server,
                name=tc.name,
                call_index=i,
                started_at=tc.started_at,
                status=tc.status,
                args_hash=args_hash,
                signature_hash=signature_hash,
                args_bytes=args_bytes,
                result_bytes=tc.result_bytes,
                model=call.model,
                command_families=list(tc.command_families[:3]),
            )
        )

    return event, tool_calls


def content_hash_tc(src_event_id: str, index: int) -> str:
    """The 16-char content hash used in a tool call's ``external_call_id``.

    ``content_hash`` already truncates to 32 chars; the tool-call id takes the
    first 16 of that, matching the Rust ``content_hash(...)[..16]``.
    """
    return wire.content_hash([src_event_id, str(index)])[:16]


def build_batch(
    cfg: Config, calls: Iterable[LlmCall], seq: int
) -> Tuple[IngestBatch, int]:
    """Drain a batch of captured calls into a wire :class:`IngestBatch`.

    ``seq`` is a monotonic counter used to keep per-call dedupe keys distinct
    within a run; it is bumped once per call. Returns the built batch and the
    updated ``seq`` (Python ints are immutable, so the new value is returned
    rather than mutated in place).
    """
    events: List[RawEvent] = []
    tool_calls: List[ToolCallWire] = []
    source_ids: List[str] = []
    # session_id -> the account that produced it. For an SDK integration the
    # account IS the app: the key's usage is this service's usage, so naming it
    # here attributes the session at ingest instead of leaving it to a
    # server-side guess.
    session_installs: Dict[str, wire.SessionInstall] = {}

    for call in calls:
        seq += 1
        if call.provider and call.session_id:
            session_installs[call.session_id] = wire.SessionInstall(
                provider_account_id=cfg.app,
                provider=call.provider,
            )
        event, tcs = _event_from_call(cfg, call, seq)
        source_ids.append(event.source_event_id)
        tool_calls.extend(tcs)
        events.append(event)

    batch = IngestBatch(
        batch_id=wire.batch_id(source_ids),
        device_id=cfg.device_id,
        app=cfg.app,
        daemon_version=cfg.client_version,
        events=events,
        tool_calls=tool_calls,
        session_installs=session_installs,
        # Always send an explicit value reflecting the config so backend usage is
        # off-by-default but users can opt in.
        auto_taxonomy=cfg.auto_taxonomy,
    )
    return batch, seq
