"""``wrap()`` -- a drop-in that auto-records each LLM call so you never
hand-build an :class:`~modelstat.LlmCall`.

Wrap an OpenAI or Anthropic client and use it exactly as before; every
completion call is forwarded untouched, and *after* it returns the SDK extracts
provider / model / token usage / text and :meth:`Client.record`s it. Recording
is **best-effort**: any error in extraction or recording is swallowed so the
wrapped call's result and timing are never affected.

This module is additive and dynamic -- it does **not** import ``openai`` or
``anthropic`` (they are optional peers). Detection is by the client's type /
module name, so ``wrap`` works whether or not either SDK is installed. Sync
clients are fully supported; ``AsyncOpenAI`` / ``AsyncAnthropic`` are supported
too (the completion coroutine is awaited and recorded on completion).

Example
-------
.. code-block:: python

    from openai import OpenAI
    import modelstat
    from modelstat import Client, Config

    ms = Client(Config("msk_live_...", "raw_sdk_openai"))
    client = modelstat.wrap(OpenAI(), recorder=ms, metadata={"feature": "search"})

    # ... unchanged usage; auto-recorded ...
    resp = client.chat.completions.create(
        model="gpt-x", messages=[{"role": "user", "content": "hello"}]
    )
"""

from __future__ import annotations

import inspect
from typing import Any, Callable, Dict, Optional, Union

from .capture import LlmCall
from .client import Client

__all__ = ["wrap"]


def wrap(
    client: Any,
    *,
    recorder: Optional[Client] = None,
    metadata: Optional[Dict[str, str]] = None,
    session_id: Union[str, Callable[[], str], None] = None,
) -> Any:
    """Wrap an OpenAI/Anthropic ``client`` so each completion call is
    auto-recorded into a modelstat :class:`Client`.

    The modelstat client is supplied via ``recorder=``. ``metadata`` is a
    wrap-default merged into every recorded call (the per-call layer; ``Config``
    defaults and the ambient ``with modelstat.metadata(...)`` layer still apply
    underneath). ``session_id`` sets the grouping id (a string or a zero-arg
    callable evaluated per call; defaults to ``"wrap"``).

    Returns a transparent proxy: attribute access forwards to ``client`` and the
    completion method (``chat.completions.create`` for OpenAI, ``messages.create``
    for Anthropic) is intercepted. Raises :class:`TypeError` if the client flavor
    can't be detected, or if no ``recorder`` is supplied.
    """
    if recorder is None:
        raise TypeError(
            "modelstat.wrap: pass the modelstat Client as `recorder=`, "
            "e.g. modelstat.wrap(openai_client, recorder=ms)."
        )
    provider = _detect_provider(client)
    if provider is None:
        raise TypeError(
            "modelstat.wrap: could not detect an OpenAI or Anthropic client "
            "(expected `.chat.completions.create` or `.messages.create`)."
        )
    opts = _Options(recorder, provider, metadata, session_id)
    return _Proxy(client, opts, ())


# ---- detection --------------------------------------------------------------


def _detect_provider(client: Any) -> Optional[str]:
    """Detect ``"openai"`` or ``"anthropic"`` from the client's completion
    method shape (works for sync and async clients alike). Returns ``None`` when
    neither matches."""
    chat = getattr(client, "chat", None)
    if chat is not None:
        completions = getattr(chat, "completions", None)
        if completions is not None and callable(
            getattr(completions, "create", None)
        ):
            return "openai"
    messages = getattr(client, "messages", None)
    if messages is not None and callable(getattr(messages, "create", None)):
        return "anthropic"
    return None


# The dotted path to the intercepted completion method, per provider.
_COMPLETION_PATH = {
    "openai": ("chat", "completions", "create"),
    "anthropic": ("messages", "create"),
}


class _Options:
    """Resolved wrap options shared across the proxy chain."""

    __slots__ = ("recorder", "provider", "metadata", "session_id")

    def __init__(
        self,
        recorder: Client,
        provider: str,
        metadata: Optional[Dict[str, str]],
        session_id: Union[str, Callable[[], str], None],
    ) -> None:
        self.recorder = recorder
        self.provider = provider
        self.metadata = metadata
        self.session_id = session_id


# ---- transparent proxy ------------------------------------------------------


class _Proxy:
    """A transparent attribute-forwarding proxy.

    Reads forward to the wrapped object; the prefix on the way to the completion
    method stays proxied so the next access is also wrapped, and the terminal
    ``create`` is replaced by an interceptor.
    """

    __slots__ = ("_target", "_opts", "_path")

    def __init__(self, target: Any, opts: _Options, path: tuple) -> None:
        object.__setattr__(self, "_target", target)
        object.__setattr__(self, "_opts", opts)
        object.__setattr__(self, "_path", path)

    def __getattr__(self, name: str) -> Any:
        target = object.__getattribute__(self, "_target")
        opts: _Options = object.__getattribute__(self, "_opts")
        path = object.__getattribute__(self, "_path")
        value = getattr(target, name)

        here = path + (name,)
        completion_path = _COMPLETION_PATH[opts.provider]

        if callable(value) and here == completion_path:
            return _intercept_create(target, value, opts)
        # On the prefix to the terminal method: keep proxying.
        if here == completion_path[: len(here)]:
            return _Proxy(value, opts, here)
        return value

    def __setattr__(self, name: str, value: Any) -> None:
        setattr(object.__getattribute__(self, "_target"), name, value)


def _intercept_create(
    target: Any, create: Callable[..., Any], opts: _Options
) -> Callable[..., Any]:
    """Wrap the terminal ``create``: run the real call, then best-effort record
    from the request kwargs + the response. Supports both sync and async
    (coroutine-returning) ``create`` methods, and never lets a recording error
    escape or alter the return value."""

    if inspect.iscoroutinefunction(create):

        async def awrapped(*args: Any, **kwargs: Any) -> Any:
            response = await create(*args, **kwargs)
            _safe_record(opts, args, kwargs, response)
            return response

        return awrapped

    def wrapped(*args: Any, **kwargs: Any) -> Any:
        response = create(*args, **kwargs)
        _safe_record(opts, args, kwargs, response)
        return response

    return wrapped


def _safe_record(
    opts: _Options, args: tuple, kwargs: dict, response: Any
) -> None:
    """Best-effort extract + record, wrapped so a malformed response or a
    recording fault can never break the user's call."""
    try:
        call = _build_call(opts, args, kwargs, response)
        opts.recorder.record(call)
    except Exception:
        # Swallow -- recording is best-effort and must never surface.
        pass


def _build_call(
    opts: _Options, args: tuple, kwargs: dict, response: Any
) -> LlmCall:
    """Build the :class:`LlmCall` from the request kwargs and the response."""
    provider = opts.provider
    model = _resp_get(response, "model") or kwargs.get("model")

    call = LlmCall(provider, _resolve_session_id(opts.session_id))
    if isinstance(model, str):
        call.model = model

    # Tokens: OpenAI -> prompt_tokens/completion_tokens; Anthropic ->
    # input_tokens/output_tokens. Read whichever the response carries.
    usage = _resp_get(response, "usage")
    input_tokens = _num(_attr(usage, "prompt_tokens"), _attr(usage, "input_tokens"))
    output_tokens = _num(
        _attr(usage, "completion_tokens"), _attr(usage, "output_tokens")
    )
    call.tokens = _token_usage(input_tokens, output_tokens)

    prompt = _extract_prompt(provider, kwargs)
    completion = _extract_completion(provider, response)
    if prompt or completion:
        call.text(prompt, completion)

    # Wrap-default tags ride the per-call layer; ambient + Config defaults still
    # apply underneath at build time.
    if opts.metadata:
        call.with_metadata(dict(opts.metadata))
    return call


def _token_usage(input_tokens: int, output_tokens: int) -> Any:
    from .wire import TokenUsage

    return TokenUsage(input=input_tokens, output=output_tokens)


def _resolve_session_id(session_id: Union[str, Callable[[], str], None]) -> str:
    if callable(session_id):
        try:
            return session_id()
        except Exception:
            return "wrap"
    return session_id if isinstance(session_id, str) else "wrap"


# ---- best-effort field extraction (dict- or attribute-shaped) ---------------


def _resp_get(obj: Any, key: str) -> Any:
    """Read ``key`` from a response that may be a dict or an attribute object
    (the provider SDKs return pydantic-ish models; tests pass plain dicts)."""
    if isinstance(obj, dict):
        return obj.get(key)
    return getattr(obj, key, None)


def _attr(obj: Any, key: str) -> Any:
    if obj is None:
        return None
    if isinstance(obj, dict):
        return obj.get(key)
    return getattr(obj, key, None)


def _num(*candidates: Any) -> int:
    """First candidate that is a real number, as an int; else 0."""
    for c in candidates:
        if isinstance(c, bool):
            continue
        if isinstance(c, (int, float)):
            return int(c)
    return 0


def _extract_prompt(provider: str, kwargs: dict) -> str:
    """Pull the prompt text out of the completion request kwargs. Both providers
    use ``messages=[{role, content}]``; Anthropic also has a top-level
    ``system``."""
    parts = []
    if provider == "anthropic":
        system = _flatten_content(kwargs.get("system"))
        if system:
            parts.append(system)
    messages = kwargs.get("messages")
    if isinstance(messages, list):
        for m in messages:
            text = _flatten_content(_attr(m, "content"))
            if text:
                parts.append(text)
    return "\n".join(parts)


def _extract_completion(provider: str, response: Any) -> str:
    """Pull the completion text out of the response. OpenAI:
    ``choices[].message.content``. Anthropic: ``content[].text``."""
    if provider == "openai":
        choices = _resp_get(response, "choices")
        if isinstance(choices, list):
            parts = []
            for c in choices:
                message = _attr(c, "message")
                text = _flatten_content(_attr(message, "content"))
                if text:
                    parts.append(text)
            return "\n".join(parts)
        return ""
    # anthropic: content is a list of blocks; text blocks carry ``.text``.
    return _flatten_content(_resp_get(response, "content"))


def _flatten_content(content: Any) -> str:
    """Flatten a message-content value into plain text: a bare string, or a list
    of parts (``{type: "text", text}`` for both providers). Non-text parts
    (images, tool-use blocks) are ignored."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, str):
                parts.append(part)
            else:
                text = _attr(part, "text")
                if isinstance(text, str):
                    parts.append(text)
        return "".join(parts)
    return ""
