"""Tests for the redaction floor. Mirrors the Rust ``redact.rs`` test module,
plus an exhaustive per-family sweep."""

from __future__ import annotations

import unittest

from modelstat.redact import redact


def clean(s: str) -> str:
    return redact(s).text


class TestRedact(unittest.TestCase):
    def test_scrubs_each_secret_family(self) -> None:
        cases = [
            "sk-ant-0123456789abcdefghijABCDEF",
            "sk-proj-0123456789abcdefghijABCDEF",
            "AIzaSyA1234567890123456789012345678901234",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_0123456789012345678901234567890123456789",
            "xoxb-1234567890-abcdefghijkl",
            "sk_live_0123456789012345678901234567",
            "ds_live_0123456789012345678901234567890123",
        ]
        for c in cases:
            out = clean(c)
            self.assertIn("[REDACTED:", out, f"expected redaction for {c!r}")

    def test_scrubs_every_named_family_with_exact_label(self) -> None:
        # Exhaustive: one representative per floor rule -> exact placeholder.
        expectations = [
            ("sk-ant-0123456789abcdefghijABCDEF", "[REDACTED:anthropic_key]"),
            ("sk-proj-0123456789abcdefghijABCDEF", "[REDACTED:openai_key]"),
            ("sk-0123456789abcdefghijABCDEF", "[REDACTED:openai_key]"),
            (
                "AIzaSyA1234567890123456789012345678901234",
                "[REDACTED:google_api_key]",
            ),
            ("AKIAIOSFODNN7EXAMPLE", "[REDACTED:aws_access_key]"),
            ("ASIAIOSFODNN7EXAMPLE", "[REDACTED:aws_access_key]"),
            (
                "ghp_0123456789012345678901234567890123456789",
                "[REDACTED:github_pat]",
            ),
            (
                "gho_0123456789012345678901234567890123456789",
                "[REDACTED:github_oauth]",
            ),
            (
                "ghs_0123456789012345678901234567890123456789",
                "[REDACTED:github_app]",
            ),
            ("xoxb-1234567890-abcdefghijkl", "[REDACTED:slack_token]"),
            (
                "sk_live_0123456789012345678901234567",
                "[REDACTED:stripe_live_key]",
            ),
            (
                "pk_test_0123456789012345678901234567",
                "[REDACTED:stripe_test_key]",
            ),
            (
                "ds_live_0123456789012345678901234567890123",
                "[REDACTED:modelstat_device_secret]",
            ),
        ]
        for raw, label in expectations:
            out = clean(raw)
            self.assertIn(label, out, f"{raw!r} -> expected {label}, got {out!r}")
            # The raw secret body must not survive.
            self.assertNotIn(raw, out)

    def test_redacts_jwt_and_discord_and_private_key(self) -> None:
        jwt = "eyJhbGciOiJIUzI1Ni9999.eyJzdWIiOiIxMjM0NTY3ODk0444.SflKxwRJSMeKKF2QT4f"
        self.assertIn("[REDACTED:jwt]", clean(jwt))

        discord = "N" + "A" * 23 + ".ABCDEF." + "a" * 27
        self.assertIn("[REDACTED:discord_token]", clean(discord))

        pem = (
            "-----BEGIN RSA PRIVATE KEY-----\n"
            "MIIEowIBAAKCAQEA...\n"
            "-----END RSA PRIVATE KEY-----"
        )
        out = clean(pem)
        self.assertIn("[REDACTED:private_key]", out)
        self.assertNotIn("MIIEowIBAAKCAQEA", out)

    def test_keeps_env_var_name_but_drops_value(self) -> None:
        out = clean("MY_API_TOKEN=supersecretvalue123")
        self.assertIn("MY_API_TOKEN=", out)
        self.assertIn("[REDACTED:env_secret]", out)
        self.assertNotIn("supersecretvalue123", out)

    def test_redacts_bearer_and_db_password(self) -> None:
        b = clean("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123")
        self.assertIn("Bearer [REDACTED:bearer]", b)

        d = clean("postgres://app:hunter2hunter2@db.internal:5432/prod")
        self.assertIn("[REDACTED:db_password]", d)
        self.assertNotIn("hunter2hunter2", d)
        # Scheme + <user> placeholder preserved.
        self.assertIn("postgres://<user>:", d)

    def test_redacts_email_and_paths_as_pii(self) -> None:
        r = redact(
            "ping me at jane.doe@example.com from /Users/jane/secret/app.py"
        )
        self.assertIn("[REDACTED:email]", r.text)
        self.assertIn("[REDACTED:path]", r.text)
        self.assertEqual(r.pii, 2)

    def test_redacts_linux_and_windows_paths(self) -> None:
        self.assertIn("[REDACTED:path]", clean("see /home/bob/.ssh/config now"))
        self.assertIn(
            "[REDACTED:path]", clean(r"open C:\Users\bob\secret.txt please")
        )

    def test_leaves_clean_text_untouched_and_counts_zero(self) -> None:
        r = redact("refactor the auth module and add tests")
        self.assertEqual(r.text, "refactor the auth module and add tests")
        self.assertEqual(r.secrets, 0)
        self.assertEqual(r.pii, 0)

    def test_aws_secret_blob_is_caught(self) -> None:
        # The canonical 40-char AWS secret-key example, standing alone.
        key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        self.assertEqual(len(key), 40)
        out = clean(f"aws_secret = {key}")
        self.assertIn("[REDACTED:aws_secret_key]", out)
        self.assertNotIn(key, out)

    def test_counts_reflect_matches(self) -> None:
        # Two emails -> pii == 2; one key -> secrets >= 1.
        r = redact("a@b.com and c@d.com with sk-ant-0123456789abcdefghijABCDEF")
        self.assertEqual(r.pii, 2)
        self.assertGreaterEqual(r.secrets, 1)


if __name__ == "__main__":
    unittest.main()
