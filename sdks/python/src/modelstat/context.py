"""The ambient metadata context layer.

``with modelstat.metadata({...}): ...`` contributes attribution tags to the
**middle** tier -- above :attr:`Config.metadata` defaults, below per-call tags --
for any :meth:`Client.record` (or :func:`modelstat.wrap` call) that happens
inside the block. The scope auto-resets when the block exits, including on an
exception, because the tags live in a :class:`contextvars.ContextVar` whose token
is reset in a ``finally``.

Nested blocks merge (an inner block sees the outer tags too), with the inner
value winning on a shared key. Built on :mod:`contextvars`, so the tags are
also correctly scoped per-task under :mod:`asyncio`.
"""

from __future__ import annotations

import contextvars
from contextlib import contextmanager
from typing import Dict, Iterator, Mapping, Optional

__all__ = ["metadata", "ambient_metadata"]

# Holds the ambient tag map for the current context. ``None`` means "no active
# block"; an empty/populated dict means a block is active.
_ambient: "contextvars.ContextVar[Optional[Dict[str, str]]]" = contextvars.ContextVar(
    "modelstat_ambient_metadata", default=None
)


@contextmanager
def metadata(tags: Mapping[str, str]) -> Iterator[None]:
    """Run the ``with`` body with ``tags`` in the ambient metadata layer.

    Tags from an enclosing :func:`metadata` block are inherited and merged, with
    ``tags`` winning on shared keys. The previous ambient tags are restored on
    exit (including on exception).
    """
    parent = _ambient.get()
    merged: Dict[str, str] = dict(parent) if parent else {}
    merged.update(tags)
    token = _ambient.set(merged)
    try:
        yield
    finally:
        _ambient.reset(token)


def ambient_metadata() -> Optional[Dict[str, str]]:
    """The ambient tag map currently in scope (a shallow copy so callers can't
    mutate the live store), or ``None`` when no :func:`metadata` block is
    active."""
    current = _ambient.get()
    return dict(current) if current is not None else None
