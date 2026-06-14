/**
 * Single source of truth for pricing. Shared across every surface that
 * displays or computes a bill, so changing a number here flows through
 * everywhere (landing copy, in-app gauge, usage reporting).
 */

export const BILLION = 1_000_000_000n;
export const MILLION = 1_000_000n;

/** Plan ids. */
export type Plan = "free" | "team";

/**
 * Free plan — hard cap. When an org on `free` hits this we keep
 * accepting events for a grace window but classification is paused
 * so the usage counter can't runaway past the cap.
 */
export const FREE_INCLUDED_TOKENS = 100n * MILLION;

/** Team plan — per-seat entitlement, pooled across the org. */
export const TEAM_INCLUDED_PER_SEAT = 250n * MILLION;
export const TEAM_SEAT_PRICE_USD_CENTS = 500; // $5.00

/**
 * Overage: $25 per billion tokens. Expressed as cents per million
 * so integer math never loses precision when reporting usage to a
 * payment processor (most take integer cents as unit amounts).
 *   $25 / 1,000 M = $0.025 / M = 2.5¢ / M → 2.5 cents/M = 25 cents / 10M
 * We bill in units of 1M tokens at 2.5 cents. Where the processor
 * requires an integer unit amount, use the decimal-string variant.
 */
export const OVERAGE_CENTS_PER_MILLION = 2.5; // $0.025 / M
export const OVERAGE_USD_PER_BILLION = 25;

/**
 * Compute the current-period bill for an org, in USD cents.
 * Returns { baseCents, overageCents, totalCents, includedTokens,
 * overageTokens } so the UI can show each component.
 */
export function computeBill(opts: {
  plan: Plan;
  seatCount: number;
  tokensProcessed: bigint;
}): {
  baseCents: number;
  overageCents: number;
  totalCents: number;
  includedTokens: bigint;
  overageTokens: bigint;
} {
  if (opts.plan === "free") {
    return {
      baseCents: 0,
      overageCents: 0,
      totalCents: 0,
      includedTokens: FREE_INCLUDED_TOKENS,
      overageTokens: 0n,
    };
  }
  const seatCount = BigInt(Math.max(1, opts.seatCount));
  const included = TEAM_INCLUDED_PER_SEAT * seatCount;
  const overage =
    opts.tokensProcessed > included ? opts.tokensProcessed - included : 0n;
  const baseCents = Number(seatCount) * TEAM_SEAT_PRICE_USD_CENTS;
  // Overage: integer-rounded in cents, to the nearest million.
  const overageMillions = Number(overage / MILLION); // lossy but OK for display
  const overageCents = Math.round(overageMillions * OVERAGE_CENTS_PER_MILLION);
  return {
    baseCents,
    overageCents,
    totalCents: baseCents + overageCents,
    includedTokens: included,
    overageTokens: overage,
  };
}

export function formatUsd(cents: number): string {
  const dollars = cents / 100;
  return `$${dollars.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

export function formatTokens(n: bigint | number): string {
  const v = typeof n === "bigint" ? Number(n) : n;
  if (v >= 1e9) return `${(v / 1e9).toFixed(v >= 1e10 ? 0 : 1)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(v >= 1e7 ? 0 : 1)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(0)}K`;
  return v.toString();
}
