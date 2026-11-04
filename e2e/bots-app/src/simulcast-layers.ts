import type { Resolved } from "./run-record";
import { PAGE_CALL_TIME_LIMIT_MS } from "./time-limit";

export const SIMULCAST_MAX_LAYERS_ENV = "BOT_SIMULCAST_MAX_LAYERS";

export const SIMULCAST_MAX_LAYERS_CONFIG_KEY = "experimentalSimulcastMaxLayers";

export const SIMULCAST_LADDER_DEPTH = 3;

/** `--simulcast-max-layers` / `BOT_SIMULCAST_MAX_LAYERS`: unset or empty inherit; 1..ladder depth override. */
export function resolveSimulcastMaxLayers(raw: string | undefined): Resolved<number | undefined> {
  const token = raw?.trim() ?? "";
  if (token === "") return { kind: "ok", value: undefined };
  const n = /^\d+$/.test(token) ? Number.parseInt(token, 10) : NaN;
  if (n >= 1 && n <= SIMULCAST_LADDER_DEPTH) return { kind: "ok", value: n };
  return {
    kind: "invalid",
    message: `--simulcast-max-layers (or ${SIMULCAST_MAX_LAYERS_ENV}) must be an integer from 1 to ${SIMULCAST_LADDER_DEPTH}, got "${raw}"`,
  };
}

/** The read-back did not complete (timed out or threw), so the ceiling is unknown. */
export const SIMULCAST_READBACK_UNREAD = "unread";

export type SimulcastMaxLayersObserved = number | null | typeof SIMULCAST_READBACK_UNREAD;

/** The value read back from `__APP_CONFIG`; `null` when absent or not a number. */
export function simulcastMaxLayersReadback(raw: unknown): number | null {
  return typeof raw === "number" ? raw : null;
}

export const SIMULCAST_READBACK_TIMEOUT_MS = PAGE_CALL_TIME_LIMIT_MS;

export class SimulcastMaxLayersReadbackTimeoutError extends Error {
  constructor(readonly timeoutMs: number) {
    super(
      `__APP_CONFIG.${SIMULCAST_MAX_LAYERS_CONFIG_KEY} read-back did not settle within ${timeoutMs}ms of joining: the renderer is stalled, so this bot cannot be verified or controlled`,
    );
    this.name = "SimulcastMaxLayersReadbackTimeoutError";
  }
}

export class SimulcastMaxLayersNotAppliedError extends Error {
  constructor(
    readonly requested: number,
    readonly observed: SimulcastMaxLayersObserved,
  ) {
    super(
      `--simulcast-max-layers ${requested} requested but __APP_CONFIG.${SIMULCAST_MAX_LAYERS_CONFIG_KEY} read back ${JSON.stringify(observed)} after join: refusing to run a bot whose video layer ceiling is not the one asked for`,
    );
    this.name = "SimulcastMaxLayersNotAppliedError";
  }
}
