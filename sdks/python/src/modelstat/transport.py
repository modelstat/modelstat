"""How a built batch leaves the worker.

The :class:`Transport` protocol lets tests run the whole pipeline in-process
(via :class:`FakeTransport`) and lets the daemon / server paths share one
worker. The real transport uses stdlib :mod:`urllib.request` so the runtime
dependency footprint stays at a single package (``blake3``) -- no HTTP client
dependency. Sending blocks, which is fine: it only ever runs on the background
worker thread, never the caller's hot path.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from threading import Lock
from typing import Any, Dict, List

from .config import Config
from .wire import IngestBatch

__all__ = ["TransportError", "Transport", "FakeTransport", "HttpTransport"]


class TransportError(Exception):
    """A transport failure. The worker retries once, then drops the batch (in
    local-daemon mode the daemon owns durable retry)."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.status = status


class Transport:
    """Ships a built batch to its destination.

    A minimal interface (duck-typed): any object with a ``send(batch_dict)``
    method that returns ``None`` on success and raises :class:`TransportError`
    on failure works as a transport.
    """

    def send(self, batch: Dict[str, Any]) -> None:  # pragma: no cover - interface
        raise NotImplementedError


class FakeTransport(Transport):
    """In-memory transport for tests: records every batch it is handed."""

    def __init__(self) -> None:
        self._batches: List[Dict[str, Any]] = []
        self._lock = Lock()

    def send(self, batch: Dict[str, Any]) -> None:
        with self._lock:
            self._batches.append(batch)

    def batches(self) -> List[Dict[str, Any]]:
        """Snapshot of every batch sent so far (as serialized wire dicts)."""
        with self._lock:
            return list(self._batches)


class HttpTransport(Transport):
    """The real HTTP transport: ``POST <endpoint>`` with a bearer ingest key."""

    def __init__(self, endpoint: str, bearer: str, timeout: float = 10.0) -> None:
        self._endpoint = endpoint
        self._bearer = bearer
        self._timeout = timeout

    @classmethod
    def from_config(cls, cfg: Config) -> "HttpTransport":
        return cls(endpoint=cfg.mode.endpoint(), bearer=cfg.ingest_key)

    def send(self, batch: Dict[str, Any]) -> None:
        body = json.dumps(batch).encode("utf-8")
        req = urllib.request.Request(
            self._endpoint,
            data=body,
            method="POST",
            headers={
                "Authorization": f"Bearer {self._bearer}",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                status = resp.status
                if not (200 <= status < 300):
                    raise TransportError(f"http status {status}", status=status)
        except urllib.error.HTTPError as e:
            # A non-2xx response surfaces here; preserve the status code.
            raise TransportError(f"http status {e.code}", status=e.code) from e
        except urllib.error.URLError as e:
            raise TransportError(f"transport: {e.reason}") from e
        except OSError as e:  # connection refused, timeout, DNS, ...
            raise TransportError(f"transport: {e}") from e
