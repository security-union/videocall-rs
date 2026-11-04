/** A visibility probe or a mouse or key event: one renderer round-trip with no protocol timeout. */
export const UI_PROBE_TIME_LIMIT_MS = 2_000;

export const PAGE_CALL_TIME_LIMIT_MS = 10_000;

export const BROWSER_STEP_TIME_LIMIT_MS = 30_000;

export class TimeLimitExceededError extends Error {
  constructor(
    readonly what: string,
    readonly limitMs: number,
  ) {
    super(`${what} did not settle within ${limitMs}ms`);
    this.name = "TimeLimitExceededError";
  }
}

/** Settles as `work()` does, or rejects with {@link TimeLimitExceededError} after `limitMs`. */
export function withTimeLimit<T>(
  what: string,
  limitMs: number,
  work: () => Promise<T>,
): Promise<T> {
  let running: Promise<T>;
  try {
    running = Promise.resolve(work());
  } catch (e) {
    return Promise.reject(e);
  }
  running.catch(() => {});
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new TimeLimitExceededError(what, limitMs)), limitMs);
  });
  return Promise.race([running, expired]).finally(() => clearTimeout(timer));
}

/** A probe's `.catch`: reads as not visible, and reports a timeout through `warn`. */
export function notVisible(warn: (message: string) => void): (e: unknown) => false {
  return (e) => {
    if (e instanceof TimeLimitExceededError) warn(`${e.message}; read as not visible`);
    return false;
  };
}
