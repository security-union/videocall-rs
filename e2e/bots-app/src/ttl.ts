/**
 * Time-to-live for a bot, expressed either as a finite millisecond count or
 * as the literal sentinel `"infinite"` for bots that run until SIGTERM.
 */
export type Ttl = number | "infinite";

const SUFFIX_MS: Record<string, number> = {
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
};

/**
 * Parse a TTL string into a {@link Ttl}.
 *
 * Accepted forms:
 *   - `"infinite"` → the sentinel `"infinite"`.
 *   - `<positive-integer><s|m|h>` (e.g. `"5m"`, `"30s"`, `"2h"`) → milliseconds.
 *
 * Whitespace and case are tolerated (`"  5M  "` is the same as `"5m"`).
 * The empty string and any other shape are rejected.
 *
 * @throws Error with a human-readable message when the input is invalid.
 */
export function parseDuration(input: string): Ttl {
  const trimmed = input.trim().toLowerCase();
  if (trimmed === "") {
    throw new Error("ttl must not be empty");
  }
  if (trimmed === "infinite") {
    return "infinite";
  }

  const match = /^(\d+)([smh])$/.exec(trimmed);
  if (!match) {
    throw new Error(
      `ttl "${input}" is not valid; expected "<int>s", "<int>m", "<int>h", or "infinite"`,
    );
  }
  const value = Number.parseInt(match[1], 10);
  if (value <= 0) {
    throw new Error(`ttl "${input}" must be positive`);
  }
  return value * SUFFIX_MS[match[2]];
}

/**
 * Format a {@link Ttl} for display.
 */
export function formatDuration(ttl: Ttl): string {
  if (ttl === "infinite") return "infinite";
  if (ttl % SUFFIX_MS.h === 0) return `${ttl / SUFFIX_MS.h}h`;
  if (ttl % SUFFIX_MS.m === 0) return `${ttl / SUFFIX_MS.m}m`;
  return `${Math.round(ttl / SUFFIX_MS.s)}s`;
}

/**
 * Returns a promise that resolves after `ttl` milliseconds, or that never
 * resolves when `ttl === "infinite"`. The returned `cancel` function stops
 * the timer early; cancellation never causes the promise to resolve, so the
 * common pattern is to race the wait against an external shutdown signal.
 */
export function waitForTtl(ttl: Ttl): { done: Promise<void>; cancel: () => void } {
  if (ttl === "infinite") {
    return {
      done: new Promise<void>(() => {}),
      cancel: () => {},
    };
  }
  let timeout: ReturnType<typeof setTimeout> | null = null;
  const done = new Promise<void>((resolve) => {
    timeout = setTimeout(() => {
      timeout = null;
      resolve();
    }, ttl);
  });
  return {
    done,
    cancel: () => {
      if (timeout !== null) {
        clearTimeout(timeout);
        timeout = null;
      }
    },
  };
}
