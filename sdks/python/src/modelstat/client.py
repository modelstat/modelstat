"""The :class:`Client` facade.

A thin handle over the background :class:`Worker`. The hot path
(:meth:`Client.record`) does nothing but a non-blocking enqueue and returns; the
worker thread redacts, batches, and ships off the request path. On overflow the
newest record is dropped and a counter increments -- your request is never
blocked and never grows memory unbounded.
"""

from __future__ import annotations

from types import TracebackType
from typing import Optional, Type

from .capture import LlmCall
from .config import Config
from .transport import HttpTransport, Transport
from .worker import Worker

__all__ = ["Client"]


class Client:
    """The SDK entry point.

    Construct with :class:`Client` (real HTTP transport for ``cfg.mode``) or
    :meth:`Client.with_transport` (a custom transport, e.g. ``FakeTransport`` in
    tests). Usable as a context manager -- ``with Client(cfg) as ms: ...`` calls
    :meth:`shutdown` on exit.
    """

    def __init__(self, cfg: Config) -> None:
        self._worker = Worker(cfg, HttpTransport.from_config(cfg))

    @classmethod
    def with_transport(cls, cfg: Config, transport: Transport) -> "Client":
        """Start the SDK with a custom :class:`Transport`."""
        self = cls.__new__(cls)
        self._worker = Worker(cfg, transport)
        return self

    def record(self, call: LlmCall) -> None:
        """Record a captured call. **Hot path:** a non-blocking enqueue. If the
        buffer is full the call is dropped and :meth:`dropped` increments -- the
        caller is never blocked."""
        self._worker.record(call)

    def dropped(self) -> int:
        """Number of calls dropped due to buffer overflow (a backpressure
        signal)."""
        return self._worker.dropped()

    def flush(self) -> None:
        """Flush buffered calls and block until the worker has shipped them."""
        self._worker.flush()

    def shutdown(self) -> None:
        """Flush on the way out, then join the worker thread."""
        self._worker.shutdown()

    # ---- context-manager sugar ---------------------------------------------

    def __enter__(self) -> "Client":
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        self.shutdown()
