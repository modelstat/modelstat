"""The background worker: the only place redaction, batching, and network I/O
happen.

It drains a bounded queue on a timer or when a batch fills, converts captured
calls into a wire batch, and ships it via the :class:`Transport`. It runs on a
single daemon thread so it never keeps the interpreter alive at shutdown, and so
the caller's hot path (:meth:`Client.record`) only ever does a non-blocking
enqueue.
"""

from __future__ import annotations

import queue
import sys
import threading
import time
from typing import List, Optional, Union

from . import capture
from .capture import LlmCall
from .config import Config
from .context import ambient_metadata
from .transport import Transport, TransportError

__all__ = ["Worker"]

# Retry the failed send once after this delay before dropping the batch.
_RETRY_DELAY = 0.25


class _Drain:
    """A queue sentinel asking the worker to flush, with an :class:`Event` the
    worker sets once the flush has been attempted (used by ``flush()`` to block
    until the buffer has been drained and shipped)."""

    __slots__ = ("done",)

    def __init__(self) -> None:
        self.done = threading.Event()


class _Shutdown:
    """A queue sentinel asking the worker to do a final flush and exit."""

    __slots__ = ("done",)

    def __init__(self) -> None:
        self.done = threading.Event()


# What can travel through the queue: a captured call, or a control sentinel.
_Msg = Union[LlmCall, _Drain, _Shutdown]


class Worker:
    """Owns the bounded queue, the background thread, and the dropped counter."""

    def __init__(self, cfg: Config, transport: Transport) -> None:
        self._cfg = cfg
        self._transport = transport
        # Bounded buffer between the hot path and the worker.
        self._queue: "queue.Queue[_Msg]" = queue.Queue(maxsize=cfg.buffer_capacity)
        # Thread-safe overflow counter (a backpressure signal).
        self._dropped = 0
        self._dropped_lock = threading.Lock()
        self._seq = 0
        self._buf: List[LlmCall] = []
        self._thread = threading.Thread(
            target=self._run, name="modelstat-worker", daemon=True
        )
        self._thread.start()

    # ---- hot path -----------------------------------------------------------

    def record(self, call: LlmCall) -> None:
        """Non-blocking enqueue. On overflow the *newest* record is dropped and
        the dropped counter increments -- the caller is never blocked and never
        does I/O or redaction here."""
        # Snapshot the ambient metadata layer here, on the hot path, while any
        # ``with modelstat.metadata(...)`` block is still active -- the actual
        # merge runs later on the worker thread, outside that block. Only set
        # when the caller hasn't already pinned a snapshot.
        if call.ambient_metadata is None:
            call.ambient_metadata = ambient_metadata()
        try:
            self._queue.put_nowait(call)
        except queue.Full:
            with self._dropped_lock:
                self._dropped += 1

    def dropped(self) -> int:
        """Number of calls dropped due to buffer overflow."""
        with self._dropped_lock:
            return self._dropped

    # ---- control ------------------------------------------------------------

    def flush(self) -> None:
        """Flush buffered calls and block until the worker has shipped them."""
        drain = _Drain()
        # ``put`` (blocking) so a full queue can't lose the control message.
        self._queue.put(drain)
        drain.done.wait()

    def shutdown(self) -> None:
        """Final flush, then join the worker thread."""
        shutdown = _Shutdown()
        self._queue.put(shutdown)
        shutdown.done.wait()
        self._thread.join()

    # ---- worker loop --------------------------------------------------------

    def _run(self) -> None:
        # Deadline of the next time-based flush. We poll the queue with a
        # timeout so an idle SDK wakes on the flush interval and a busy one
        # flushes as soon as a batch fills -- the equivalent of the Rust
        # select! over a channel and a ticker.
        next_flush = time.monotonic() + self._cfg.flush_interval
        while True:
            timeout = max(0.0, next_flush - time.monotonic())
            try:
                msg: Optional[_Msg] = self._queue.get(timeout=timeout)
            except queue.Empty:
                msg = None

            if msg is None:
                # Timer elapsed.
                self._flush()
                next_flush = time.monotonic() + self._cfg.flush_interval
                continue

            if isinstance(msg, _Drain):
                self._flush()
                msg.done.set()
                next_flush = time.monotonic() + self._cfg.flush_interval
                continue

            if isinstance(msg, _Shutdown):
                self._flush()
                msg.done.set()
                return

            # A captured call.
            self._buf.append(msg)
            if len(self._buf) >= self._cfg.flush_max_batch:
                self._flush()
                next_flush = time.monotonic() + self._cfg.flush_interval

    def _flush(self) -> None:
        """Convert and ship the buffered calls. Retries once on failure, then
        drops the batch loudly (in local-daemon mode the daemon owns durable
        retry; remote durability is a follow-up -- see the README)."""
        if not self._buf:
            return
        calls = self._buf
        self._buf = []
        batch, self._seq = capture.build_batch(self._cfg, calls, self._seq)
        payload = batch.to_dict()

        for attempt in range(2):
            try:
                self._transport.send(payload)
                return
            except TransportError as e:
                if attempt == 0:
                    print(
                        f"modelstat: send failed (retrying once): {e}",
                        file=sys.stderr,
                    )
                    time.sleep(_RETRY_DELAY)
                else:
                    print(
                        f"modelstat: dropping batch of {len(batch.events)} "
                        f"events after retry: {e}",
                        file=sys.stderr,
                    )
            except Exception as e:  # never let the worker thread die
                if attempt == 0:
                    print(
                        f"modelstat: send error (retrying once): {e}",
                        file=sys.stderr,
                    )
                    time.sleep(_RETRY_DELAY)
                else:
                    print(
                        f"modelstat: dropping batch of {len(batch.events)} "
                        f"events after retry: {e}",
                        file=sys.stderr,
                    )
