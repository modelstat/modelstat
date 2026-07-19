//! `paramShape` — the value-masked "shape" of a command's arguments. Byte-for-
//! byte port of TS `@modelstat/core/ids.paramShape` (feature §17.3).
//!
//! CROSS-REPO CONTRACT: the server's Rust `modelstat_core::tool_exec` reproduces
//! this bit-for-bit; the golden vectors in `tests/golden/param_shape.json` (from
//! the TS test table) pin it on this side. Rules: tokenize on ASCII whitespace
//! (` \t\n\r\f`); a `-`-leading token is a flag kept verbatim except
//! `--key=value` → `--key=§`; every other token → `§`; join with single spaces.

/// The mask character, U+00A7 SECTION SIGN.
const MASK: char = '\u{00A7}';

/// JS `String.split(/[ \t\n\r\f]+/)` splits on runs of exactly these five ASCII
/// whitespace characters — NOT the full Unicode `\s` set, and notably NOT the
/// vertical tab (`\x0B`). Matching this set exactly is part of the contract.
fn is_param_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000C}')
}

/// Deterministic value-masked skeleton of `args`. See module docs.
pub fn param_shape(args: &str) -> String {
    args.split(is_param_ws)
        .filter(|t| !t.is_empty())
        .map(mask_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn mask_token(t: &str) -> String {
    if t.starts_with('-') {
        // `--key=value` keeps the flag + `=`, masks the value; a bare flag is
        // kept verbatim. `=` is ASCII so slicing at its byte index equals the JS
        // UTF-16 `slice(0, eq+1)` (both cut through the same first `=`).
        match t.find('=') {
            Some(pos) => format!("{}{}", &t[..=pos], MASK),
            None => t.to_string(),
        }
    } else {
        MASK.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors packages/core/src/ids.test.ts (golden vectors also in
    // tests/golden/param_shape.json).
    #[test]
    fn golden_vectors() {
        assert_eq!(
            param_shape("rollout restart deploy/payments-api -n prod"),
            "§ § § -n §"
        );
        assert_eq!(param_shape("commit -m \"fix bug\""), "§ -m § §");
        assert_eq!(param_shape("-la /etc/passwd"), "-la §");
        assert_eq!(param_shape("install react"), "§ §");
        assert_eq!(
            param_shape("--namespace=prod get pods"),
            "--namespace=§ § §"
        );
        assert_eq!(param_shape("run --watch -j4 test/unit"), "§ --watch -j4 §");
        assert_eq!(param_shape("status"), "§");
        assert_eq!(param_shape(""), "");
        assert_eq!(param_shape("   "), "");
    }
}
