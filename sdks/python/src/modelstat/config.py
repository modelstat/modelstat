"""SDK configuration: where to ship, how to authenticate, how hard to redact,
and how the background worker batches.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, Optional

from ._version import __version__

__all__ = ["Mode", "RedactionPolicy", "Config", "DEFAULT_DAEMON_URL"]

# The default local daemon loopback URL.
DEFAULT_DAEMON_URL = "http://127.0.0.1:4319/v1/ingest"


@dataclass(frozen=True)
class Mode:
    """Where the SDK ships captured calls.

    Construct via :meth:`local_daemon` or :meth:`remote` rather than directly.
    A "local daemon" mode hands calls to a local modelstat daemon over loopback;
    the daemon summarizes with its local Qwen model and ships only redacted
    abstracts to the server -- raw text never leaves the machine. A "remote"
    mode ships directly to the modelstat server (no local daemon / no local
    model); with ``raw = True`` it sends full (still floor-redacted) turns to
    ``/v1/ingest/raw`` for server-side summarization.
    """

    # ``"local_daemon"`` or ``"remote"``.
    kind: str
    # The daemon's loopback ingest URL (local-daemon mode only).
    url: Optional[str] = None
    # Base URL, e.g. ``https://api.modelstat.ai`` (remote mode only).
    base_url: Optional[str] = None
    # When ``True``, remote mode sends full floor-redacted turns to
    # ``/v1/ingest/raw`` for server-side summarization; when ``False``, only the
    # floor-redacted <=320-char excerpt to ``/v1/ingest``.
    raw: bool = False

    @classmethod
    def local_daemon(cls, url: str = DEFAULT_DAEMON_URL) -> "Mode":
        """Hand off to a local modelstat daemon over loopback (the default)."""
        return cls(kind="local_daemon", url=url)

    @classmethod
    def remote(cls, base_url: str, raw: bool = False) -> "Mode":
        """Ship directly to the modelstat server (no local daemon)."""
        return cls(kind="remote", base_url=base_url, raw=raw)

    def endpoint(self) -> str:
        """Resolve the concrete POST endpoint for this mode."""
        if self.kind == "local_daemon":
            assert self.url is not None
            return self.url
        # remote
        assert self.base_url is not None
        base = self.base_url.rstrip("/")
        return f"{base}/v1/ingest/raw" if self.raw else f"{base}/v1/ingest"


class RedactionPolicy(Enum):
    """How hard to scrub text before it leaves the SDK process."""

    # Run the privacy floor (secrets + email + absolute paths). The default, and
    # the floor that even "raw" mode keeps.
    FLOOR = "floor"
    # Skip in-process redaction entirely. Only valid when shipping to a trusted
    # local daemon that will redact, or under an explicit raw-data contract.
    NONE = "none"


@dataclass
class Config:
    """SDK configuration.

    Construct with the two required arguments (``ingest_key`` and ``agent``),
    then adjust fields directly or use the ``with_*`` helpers. Defaults:
    local-daemon mode, floor redaction, a 4096-slot buffer, a 2s flush interval,
    and 256-record batches.
    """

    # Bearer credential: an org-scoped ingest key (``msk_...``) or a device
    # secret.
    ingest_key: str
    # The **agent** label for every record -- which AI tool/integration the user
    # used (e.g. ``raw_sdk_openai``, ``raw_sdk_anthropic``, ``raw_sdk_generic``).
    # Ships as the wire ``agent`` field.
    agent: str
    # Stable device/service identifier (``dev_...``). Should be stable per host
    # so dedupe keys are stable across restarts.
    device_id: str = "dev_sdk"
    # This client build's version (<=40 chars). Ships as the wire
    # ``daemon_version`` field -- the *producer's* version (daemon or SDK), not
    # the agent's.
    client_version: str = field(default_factory=lambda: f"python-sdk/{__version__}")
    # Where to ship.
    mode: Mode = field(default_factory=Mode.local_daemon)
    # In-process redaction policy.
    redaction: RedactionPolicy = RedactionPolicy.FLOOR
    # Bounded in-memory buffer between the hot path and the worker. On overflow
    # the newest record is dropped and the dropped-counter increments -- the
    # live request is never blocked.
    buffer_capacity: int = 4096
    # Flush the buffer at least this often, in seconds.
    flush_interval: float = 2.0
    # Flush eagerly once this many records are buffered.
    flush_max_batch: int = 256
    # Whether the server should run taxonomy auto-detection on batches from this
    # client. Ships as the wire ``auto_taxonomy`` field. Defaults to ``False``
    # for SDK/backend integrations -- backend LLM usage isn't interactive
    # work-sessions, so taxonomy is **off by default**; set it to ``True`` to opt
    # in.
    auto_taxonomy: bool = False
    # Constant attribution tags applied to **every** call (e.g.
    # ``{"environment": "prod", "service": "checkout"}``). The lowest-priority
    # layer: the ambient context layer (``with modelstat.metadata(...)``) and
    # per-call tags both win on a shared key. Capped before send (<=16 entries;
    # keys <=64 chars; values <=256 chars). Empty by default.
    metadata: Dict[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        # The wire field is constrained to 1..=40 chars; keep the SDK honest so
        # a long custom version can't trip an HTTP 400 at the server.
        if len(self.client_version) > 40:
            self.client_version = self.client_version[:40]

    def with_remote(self, base_url: str, raw: bool = False) -> "Config":
        """Ship directly to the modelstat server instead of a local daemon.

        ``raw = True`` opts into server-side summarization of full
        (floor-redacted) turns. Returns ``self`` for chaining.
        """
        self.mode = Mode.remote(base_url, raw)
        return self

    def with_device_id(self, device_id: str) -> "Config":
        """Override the device id. Returns ``self`` for chaining."""
        self.device_id = device_id
        return self

    def sends_full_turns(self) -> bool:
        """Whether this mode sends full (untruncated) redacted turns for
        server-side summarization (remote + raw)."""
        return self.mode.kind == "remote" and self.mode.raw
