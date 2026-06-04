/**
 * Mirrors the parseDuration / formatDuration helpers in
 * `e2e/bots-app/src/ttl.ts`. Duplicated rather than imported because
 * the dashboard subtree is fully self-contained — no node-side
 * dependency at runtime, only in the proxy.
 *
 * Keep the regex + units in sync with the Node side.
 */

export type Ttl = number | "infinite";

const SUFFIX_MS: Record<string, number> = {
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
};

export function parseDurationClient(input: string): Ttl {
  const trimmed = input.trim().toLowerCase();
  if (trimmed === "") {
    throw new Error("TTL must not be empty");
  }
  if (trimmed === "infinite") {
    return "infinite";
  }
  const match = /^(\d+)([smh])$/.exec(trimmed);
  if (!match) {
    throw new Error(`TTL "${input}" is not valid — expected "<int>s", "<int>m", "<int>h", or "infinite"`);
  }
  const value = Number.parseInt(match[1], 10);
  if (value <= 0) {
    throw new Error(`TTL "${input}" must be positive`);
  }
  return value * SUFFIX_MS[match[2]];
}

/**
 * Format `ttlRemainingMs` (as exposed by the snapshot endpoint) as a
 * compact mm:ss / hh:mm:ss string for the table column. `null`
 * (infinite TTL) renders as the em-dash sentinel.
 */
export function formatRemaining(ms: number | null): string {
  if (ms === null) return "infinite";
  if (ms <= 0) return "0s";
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const pad = (n: number) => n.toString().padStart(2, "0");
  if (h > 0) return `${h}:${pad(m)}:${pad(s)}`;
  return `${m}:${pad(s)}`;
}

export function isValidTtl(input: string): boolean {
  try {
    parseDurationClient(input);
    return true;
  } catch {
    return false;
  }
}
