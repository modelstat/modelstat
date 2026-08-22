# CLAUDE.md — modelstat daemon

Read [`AGENTS.md`](AGENTS.md) first — it is the working guide for this repo
(what the daemon does, the parser/capture rules, release lines, and gates).

## The design razor — weakest hypothesis, not shortest

Non-negotiable, for every design decision (Bennett,
[arXiv:2301.12987](https://arxiv.org/abs/2301.12987)): among designs that
EXACTLY handle the cases actually observed, choose the **weakest** — the one
committing to the least beyond the data. Generalisation is a property of a
design's **extension** (the unseen cases it still handles correctly), and the
paper proves weakness maximisation is necessary and sufficient for it, while
shortness/elegance is neither. In its experiments the weakest hypothesis
generalised at 1.1–5× the rate of the shortest.

This matters more here than anywhere: the daemon runs against tools it has
never seen, on machines it cannot inspect, and ships a release to update.

- Parsers emit raw events **verbatim** and never interpret — the server
  decides. Interpretation in the daemon is a commitment about every future
  version of every tool.
- Discovery probes by **artefact shape**, never an app-name or install-path
  allowlist.
- No allowlists for open-ended sets found in the data (model names, tool
  names, categories) — pass strings through.
- Known contracts (the redaction floor, the wire schema) ARE deliberate
  commitments — explicit, validated, enforced in one place.
- Weak ≠ vague: still decide every observed case exactly, against real
  fixtures.
- Review test: what unseen-but-plausible input would this silently mishandle,
  and what in today's data forces that commitment? Nothing forces it →
  weaken it, usually by deleting structure.

See the full section in [`AGENTS.md`](AGENTS.md) § "Design principle: the
weakest sufficient hypothesis".
