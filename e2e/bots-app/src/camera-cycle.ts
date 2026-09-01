import { ACTION_BAR_SELECTOR, CAMERA_TOOLTIP, cameraButtonSelector } from "./control-buttons";

// camera-cycle.ts — opt-in per-bot camera duty cycle (#2362). Unset ⇒ camera on
// for the whole run. Set ⇒ cameras-off time is load the run does not represent.

export const CAMERA_CYCLE_ENV = {
  onMin: "BOT_CAMERA_ON_SECS_MIN",
  onMax: "BOT_CAMERA_ON_SECS_MAX",
  offMin: "BOT_CAMERA_OFF_SECS_MIN",
  offMax: "BOT_CAMERA_OFF_SECS_MAX",
} as const;

export const CAMERA_CYCLE_ENV_NAMES = [
  CAMERA_CYCLE_ENV.onMin,
  CAMERA_CYCLE_ENV.onMax,
  CAMERA_CYCLE_ENV.offMin,
  CAMERA_CYCLE_ENV.offMax,
] as const;

/** Seconds; fail-fast ceiling on any single phase. */
export const CAMERA_CYCLE_SECS_CEILING = 86_400;

export interface CameraCycleConfig {
  onMinMs: number;
  onMaxMs: number;
  offMinMs: number;
  offMaxMs: number;
}

export type CameraCycleResult =
  | { kind: "ok"; value: CameraCycleConfig | undefined }
  | { kind: "invalid"; message: string };

export type CameraPhase = "on" | "off";

export type CameraCycleRaw = Partial<Record<keyof typeof CAMERA_CYCLE_ENV, string | undefined>>;

const FIELD_ENV: Record<keyof typeof CAMERA_CYCLE_ENV, string> = CAMERA_CYCLE_ENV;
const FIELDS = ["onMin", "onMax", "offMin", "offMax"] as const;

function invalid(message: string): CameraCycleResult {
  return { kind: "invalid", message };
}

/**
 * All four unset ⇒ `undefined` (camera always on). A PARTIAL set is rejected
 * rather than silently disabling the cycle.
 */
export function resolveCameraCycle(raw: CameraCycleRaw): CameraCycleResult {
  const trimmed = {} as Record<(typeof FIELDS)[number], string>;
  const set: string[] = [];
  const unset: string[] = [];
  for (const f of FIELDS) {
    const v = (raw[f] ?? "").trim();
    trimmed[f] = v;
    (v === "" ? unset : set).push(FIELD_ENV[f]);
  }
  if (set.length === 0) return { kind: "ok", value: undefined };
  if (unset.length > 0) {
    return invalid(
      `camera cycling needs all four of ${CAMERA_CYCLE_ENV_NAMES.join(", ")}; missing: ${unset.join(", ")}. Set them all, or none (none = camera on for the whole run).`,
    );
  }

  const secs = {} as Record<(typeof FIELDS)[number], number>;
  for (const f of FIELDS) {
    const token = trimmed[f];
    if (!/^\d{1,5}$/.test(token)) {
      return invalid(
        `${FIELD_ENV[f]} must be a positive integer of at most 5 digits (seconds), got "${token}"`,
      );
    }
    const n = Number.parseInt(token, 10);
    if (n < 1) {
      return invalid(`${FIELD_ENV[f]} must be >= 1 second, got "${token}"`);
    }
    if (n > CAMERA_CYCLE_SECS_CEILING) {
      return invalid(
        `${FIELD_ENV[f]} must be <= ${CAMERA_CYCLE_SECS_CEILING} seconds, got "${token}"`,
      );
    }
    secs[f] = n;
  }
  if (secs.onMin > secs.onMax) {
    return invalid(
      `${CAMERA_CYCLE_ENV.onMin}=${secs.onMin} must be <= ${CAMERA_CYCLE_ENV.onMax}=${secs.onMax}`,
    );
  }
  if (secs.offMin > secs.offMax) {
    return invalid(
      `${CAMERA_CYCLE_ENV.offMin}=${secs.offMin} must be <= ${CAMERA_CYCLE_ENV.offMax}=${secs.offMax}`,
    );
  }
  return {
    kind: "ok",
    value: {
      onMinMs: secs.onMin * 1_000,
      onMaxMs: secs.onMax * 1_000,
      offMinMs: secs.offMin * 1_000,
      offMaxMs: secs.offMax * 1_000,
    },
  };
}

/** Whole percent, truncating — matches the entrypoint's shell arithmetic. */
export function targetDutyPct(cfg: CameraCycleConfig): number {
  const on = cfg.onMinMs + cfg.onMaxMs;
  const off = cfg.offMinMs + cfg.offMaxMs;
  return Math.floor((on * 100) / (on + off));
}

export function formatCameraCycleConfig(cfg: CameraCycleConfig): string {
  const s = (ms: number): number => Math.round(ms / 1_000);
  return (
    `on=[${s(cfg.onMinMs)}-${s(cfg.onMaxMs)}]s off=[${s(cfg.offMinMs)}-${s(cfg.offMaxMs)}]s ` +
    `target_duty=${targetDutyPct(cfg)}%`
  );
}

/** Length of the CURRENT phase before the next toggle. `rand` is a `[0, 1)` draw. */
export function nextPhaseMs(cfg: CameraCycleConfig, phase: CameraPhase, rand: number): number {
  const [min, max] = phase === "on" ? [cfg.onMinMs, cfg.onMaxMs] : [cfg.offMinMs, cfg.offMaxMs];
  const clamped = rand >= 1 ? 0.999_999_999 : rand > 0 ? rand : 0;
  return min + Math.floor(clamped * (max - min + 1));
}

export interface CameraCycleTally {
  /** Toggles whose post-condition confirmed the button flipped. */
  confirmed: number;
  failed: number;
  onMs: number;
  offMs: number;
}

export function newCameraCycleTally(): CameraCycleTally {
  return { confirmed: 0, failed: 0, onMs: 0, offMs: 0 };
}

export function recordCameraPhase(
  tally: CameraCycleTally,
  phase: CameraPhase,
  elapsedMs: number,
): void {
  if (!Number.isFinite(elapsedMs) || elapsedMs <= 0) return;
  if (phase === "on") tally.onMs += elapsedMs;
  else tally.offMs += elapsedMs;
}

export const CAMERA_CYCLE_APPLIED_BANNER = "CAMERA_CYCLE_APPLIED";
export const CAMERA_CYCLE_DEGRADED_BANNER = "CAMERA_CYCLE_DEGRADED";
export const CAMERA_CYCLE_NEVER_FIRED_BANNER = "CAMERA_CYCLE_NEVER_FIRED";

export interface CameraCycleReceipt {
  banner: string;
  line: string;
}

/** Reports the configured cycle AND what the bot achieved, never only the former. */
export function formatCameraCycleReceipt(
  cfg: CameraCycleConfig,
  tally: CameraCycleTally,
): CameraCycleReceipt {
  const totalMs = tally.onMs + tally.offMs;
  const observed =
    totalMs > 0
      ? `observed_on=${Math.round((tally.onMs * 100) / totalMs)}% of ${Math.round(totalMs / 1_000)}s`
      : "observed_on=n/a (no measured time in meeting)";
  const head = `${formatCameraCycleConfig(cfg)} toggles=ok:${tally.confirmed}/failed:${tally.failed} ${observed}`;

  if (tally.failed > 0) {
    return {
      banner: CAMERA_CYCLE_DEGRADED_BANNER,
      line:
        `${CAMERA_CYCLE_DEGRADED_BANNER} ${head} — ${tally.failed} toggle(s) did not take effect, ` +
        `so this bot's camera duty cycle is NOT the configured one and its publish load is not representative`,
    };
  }
  if (tally.confirmed === 0) {
    return {
      banner: CAMERA_CYCLE_NEVER_FIRED_BANNER,
      line:
        `${CAMERA_CYCLE_NEVER_FIRED_BANNER} ${head} — the run ended before the first camera-off ` +
        `boundary, so this bot published camera for its whole life`,
    };
  }
  return {
    banner: CAMERA_CYCLE_APPLIED_BANNER,
    line: `${CAMERA_CYCLE_APPLIED_BANNER} ${head} — cameras-off time is real load this run did NOT represent`,
  };
}

export interface CameraCycleLocator {
  first(): CameraCycleLocator;
  hover(options?: { timeout?: number }): Promise<void>;
  isVisible(options?: { timeout?: number }): Promise<boolean>;
  click(options?: { timeout?: number }): Promise<void>;
  waitFor(options: { state?: "attached"; timeout?: number }): Promise<void>;
}

export interface CameraCyclePage {
  locator(selector: string): CameraCycleLocator;
}

export interface CameraToggleOutcome {
  ok: boolean;
  reason?: string;
}

/**
 * Toggle to `target`, then ASSERT the post-condition: the opposite tooltip is in
 * the DOM. The action bar auto-hides, so the hover is re-applied every call.
 * A failure returns `ok: false` for the receipt.
 */
export async function setCameraEnabled(
  page: CameraCyclePage,
  target: CameraPhase,
  timeoutMs: number,
): Promise<CameraToggleOutcome> {
  const current: CameraPhase = target === "on" ? "off" : "on";
  const clickSel = cameraButtonSelector(CAMERA_TOOLTIP[current]);
  const confirmSel = cameraButtonSelector(CAMERA_TOOLTIP[target]);
  try {
    await page
      .locator(ACTION_BAR_SELECTOR)
      .first()
      .hover({ timeout: timeoutMs })
      .catch(() => {});
    const btn = page.locator(clickSel);
    if (!(await btn.isVisible({ timeout: timeoutMs }).catch(() => false))) {
      return {
        ok: false,
        reason: `no visible button matching ${clickSel} — action bar autohidden, camera unavailable, or the aria-label changed`,
      };
    }
    await btn.click({ timeout: timeoutMs });
    await page.locator(confirmSel).waitFor({ state: "attached", timeout: timeoutMs });
    return { ok: true };
  } catch (e) {
    return { ok: false, reason: (e as Error).message };
  }
}

export interface CameraCycleRunner {
  /** Idempotent. */
  stop(): CameraCycleReceipt;
}

/**
 * Self-throttling `setTimeout` chain: the next phase is armed only after the
 * previous toggle settles, and the timer is `unref`'d.
 */
export function startCameraCycle(args: {
  page: CameraCyclePage;
  config: CameraCycleConfig;
  timeoutMs: number;
  log: (message: string) => void;
  error: (message: string) => void;
  random?: () => number;
}): CameraCycleRunner {
  const { page, config, timeoutMs } = args;
  const random = args.random ?? Math.random;
  const tally = newCameraCycleTally();
  let phase: CameraPhase = "on";
  let phaseStartedAt = Date.now();
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let receipt: CameraCycleReceipt | null = null;

  const schedule = (): void => {
    if (stopped) return;
    timer = setTimeout(runToggle, nextPhaseMs(config, phase, random()));
    timer.unref?.();
  };
  const runToggle = (): void => {
    if (stopped) return;
    const target: CameraPhase = phase === "on" ? "off" : "on";
    void setCameraEnabled(page, target, timeoutMs)
      .catch((e: unknown): CameraToggleOutcome => ({ ok: false, reason: String(e) }))
      .then((outcome) => {
        if (stopped) return;
        const now = Date.now();
        recordCameraPhase(tally, phase, now - phaseStartedAt);
        phaseStartedAt = now;
        if (outcome.ok) {
          tally.confirmed++;
          phase = target;
          args.log(`camera cycle: camera ${target}`);
        } else {
          tally.failed++;
          args.error(
            `camera cycle: FAILED to turn camera ${target} (${outcome.reason ?? "unknown"}) — ` +
              `the configured duty cycle is NOT being applied`,
          );
        }
        schedule();
      })
      .catch((e: unknown) => {
        if (stopped) return;
        try {
          args.error(
            `camera cycle: INTERNAL fault while recording the toggle (${String(e)}) — ` +
              `not a camera-toggle failure; the cycle is rearming`,
          );
        } catch {
          // The reporter is what threw; the rearm below is the point.
        }
        schedule();
      });
  };
  schedule();

  return {
    stop(): CameraCycleReceipt {
      if (receipt !== null) return receipt;
      stopped = true;
      if (timer !== undefined) {
        clearTimeout(timer);
        timer = undefined;
      }
      recordCameraPhase(tally, phase, Date.now() - phaseStartedAt);
      receipt = formatCameraCycleReceipt(config, tally);
      return receipt;
    },
  };
}
