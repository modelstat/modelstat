"""The privacy floor: deterministic, dependency-light redaction that runs
**in-process before any bytes leave the SDK**.

This is a Python port of the daemon's ``SECRET_FLOOR``
(``packages/core/src/redact-floor.ts``) plus the email / absolute-path PII
rules, and a faithful peer of the Rust SDK's ``redact.rs``. It is the
irreducible baseline -- even in "raw" remote mode the floor still scrubs live
credentials; "raw" means *full turns*, not *leaked keys*.

Placeholder style is **square brackets** (``[REDACTED:name]``), matching the
Rust SDK.

Parity note: unlike Rust's ``regex`` crate, Python's :mod:`re` supports
look-around, so the boundary-sensitive 40-char AWS-secret blob is expressed with
the original ``(?<!...)`` / ``(?!...)`` look-arounds rather than Rust's explicit
boundary-capture workaround. The behavior is identical; the unit tests assert
each credential family is caught.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import List, Pattern, Tuple

__all__ = ["Redacted", "redact"]


@dataclass
class Redacted:
    """Result of a redaction pass."""

    text: str
    # Count of secret-format matches replaced.
    secrets: int = 0
    # Count of PII matches replaced (emails, absolute paths).
    pii: int = 0


# Ordered specific -> generic. Specific provider keys run before the generic
# env-secret / blob catchers so a known key is labelled precisely. Each entry is
# a ``(compiled_pattern, replacement)`` pair; replacements that keep a captured
# group use the ``\g<1>`` back-reference form.
_FLOOR: List[Tuple[Pattern[str], str]] = [
    (re.compile(r"sk-ant-[A-Za-z0-9_-]{20,}"), "[REDACTED:anthropic_key]"),
    (re.compile(r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}"), "[REDACTED:openai_key]"),
    (re.compile(r"AIza[0-9A-Za-z_-]{35}"), "[REDACTED:google_api_key]"),
    (re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"), "[REDACTED:aws_access_key]"),
    (re.compile(r"ghp_[A-Za-z0-9]{36,}"), "[REDACTED:github_pat]"),
    (re.compile(r"gho_[A-Za-z0-9]{36,}"), "[REDACTED:github_oauth]"),
    (re.compile(r"gh[sur]_[A-Za-z0-9]{36,}"), "[REDACTED:github_app]"),
    (re.compile(r"xox[aboprs]-[A-Za-z0-9-]{10,}"), "[REDACTED:slack_token]"),
    (
        re.compile(r"(?:sk|pk|rk)_live_[A-Za-z0-9]{24,}"),
        "[REDACTED:stripe_live_key]",
    ),
    (
        re.compile(r"(?:sk|pk|rk)_test_[A-Za-z0-9]{24,}"),
        "[REDACTED:stripe_test_key]",
    ),
    (
        re.compile(r"[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}"),
        "[REDACTED:discord_token]",
    ),
    (
        re.compile(
            r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"
        ),
        "[REDACTED:jwt]",
    ),
    (
        re.compile(
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----"
            r"[\s\S]*?"
            r"-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----"
        ),
        "[REDACTED:private_key]",
    ),
    (
        re.compile(r"ds_live_[A-Za-z0-9_-]{32,}"),
        "[REDACTED:modelstat_device_secret]",
    ),
    # Generic env-style KEY=VALUE where KEY names a secret. Keeps the name.
    (
        re.compile(
            r"\b([A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|API)"
            r"[A-Z0-9_]*)\s*[:=]\s*['\"]?([^\s'\"]{12,})['\"]?"
        ),
        r"\g<1>=[REDACTED:env_secret]",
    ),
    (
        re.compile(r"Bearer\s+[A-Za-z0-9._~+/-]{20,}=*"),
        "Bearer [REDACTED:bearer]",
    ),
    (
        re.compile(
            r"(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?://"
            r"[^:\s]+:([^@\s]+)@",
            re.IGNORECASE,
        ),
        r"\g<1>://<user>:[REDACTED:db_password]@",
    ),
    # Most generic, LAST among secrets: the 40-char base64-ish blob (e.g. a lone
    # AWS secret access key). Look-arounds leave an embedded blob inside a longer
    # token alone -- the direct Python equivalent of the TS source.
    (
        re.compile(r"(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])"),
        "[REDACTED:aws_secret_key]",
    ),
]

# PII patterns, applied after the secret floor.
_EMAIL: Pattern[str] = re.compile(
    r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"
)
# Absolute home paths on macOS / Linux / Windows -- they leak usernames and
# machine layout.
_ABS_PATH: Pattern[str] = re.compile(
    r"(?:/Users/|/home/)[^\s\"'`)]+|[A-Za-z]:\\Users\\[^\s\"'`)]+"
)


def redact(input_text: str) -> Redacted:
    """Redact ``input_text`` against the floor.

    Returns the cleaned text and per-class counts. Each class counts its matches
    *before* replacing (mirroring the Rust reference), so the counts reflect the
    number of distinct secrets/PII scrubbed at each stage.
    """
    text = input_text
    secrets = 0
    pii = 0

    for pattern, replacement in _FLOOR:
        matches = len(pattern.findall(text))
        if matches:
            text = pattern.sub(replacement, text)
            secrets += matches

    matches = len(_EMAIL.findall(text))
    if matches:
        text = _EMAIL.sub("[REDACTED:email]", text)
        pii += matches

    matches = len(_ABS_PATH.findall(text))
    if matches:
        text = _ABS_PATH.sub("[REDACTED:path]", text)
        pii += matches

    return Redacted(text=text, secrets=secrets, pii=pii)
