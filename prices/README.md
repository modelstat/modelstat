# Placeholder price tables

The YAML files in this directory are **placeholders, not real prices.** They
exist so that `scripts/build-prices.ts` can produce a valid `prices.json`
when this repo is checked out on its own.

Every rate is a round fake number (e.g. $1 / $5 per million tokens); do not
use them to compute real costs. Replace with authoritative values before
shipping anything cost-sensitive — these are placeholders; real rates are
applied server-side.

Schema is documented in [`packages/pricing/src/index.ts`](../packages/pricing/src/index.ts).
