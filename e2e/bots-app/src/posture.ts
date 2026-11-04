import { taggedLine } from "./log-line";
import { type PinnedTransport, TRANSPORT_WS_EVERY_ENV } from "./transport";

/** Clock-mode capture geometry (#2236). */
export interface SourceGeometry {
  readonly width: number;
  readonly height: number;
}

export const SD_SOURCE: SourceGeometry = { width: 640, height: 480 };
export const HD_SOURCE: SourceGeometry = { width: 1280, height: 720 };

/** One index per stride is HD; residue 1 keeps an unset index (0) on SD. */
const HD_STRIDE = 6;
const HD_RESIDUE = 1;

function requireBotIndex(index: number): number {
  if (!Number.isSafeInteger(index) || index < 0) {
    throw new Error(`bot index must be a non-negative integer, got ${String(index)}`);
  }
  return index;
}

export function sourceGeometryForIndex(index: number): SourceGeometry {
  return requireBotIndex(index) % HD_STRIDE === HD_RESIDUE ? HD_SOURCE : SD_SOURCE;
}

/** An HD index, so every stride puts index 7 on WebSocket: a fleet of 8+ always has an HD WebSocket bot. */
const TRANSPORT_WS_ANCHOR = HD_RESIDUE + HD_STRIDE;

export function transportForIndex(index: number, wsEvery: number): PinnedTransport {
  if (!Number.isSafeInteger(wsEvery) || wsEvery < 1) {
    throw new Error(`wsEvery must be a positive integer, got ${String(wsEvery)}`);
  }
  return requireBotIndex(index) % wsEvery === TRANSPORT_WS_ANCHOR % wsEvery
    ? "websocket"
    : "webtransport";
}

export function websocketIndices(wsEvery: number, fleetSize: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < fleetSize; i++) {
    if (transportForIndex(i, wsEvery) === "websocket") out.push(i);
  }
  return out;
}

/** `BOT_TRANSPORT_WS_EVERY`: unset or empty is `null` (no stride). */
export function resolveTransportWsEvery(
  raw: string | undefined,
): { kind: "ok"; value: number | null } | { kind: "invalid"; message: string } {
  if (raw === undefined || raw.trim() === "") return { kind: "ok", value: null };
  const token = raw.trim();
  const value = Number.parseInt(token, 10);
  if (!/^\d+$/.test(token) || !Number.isSafeInteger(value) || value < 1) {
    return {
      kind: "invalid",
      message: `${TRANSPORT_WS_EVERY_ENV} must be a positive integer, got "${raw}"`,
    };
  }
  if (confoundsGeometry(value)) {
    return {
      kind: "invalid",
      message: `${TRANSPORT_WS_EVERY_ENV}=${value} divides or is a multiple of ${HD_STRIDE}, so transport is confounded with capture geometry; use a balanced stride, e.g. ${BALANCED_WS_STRIDES.join(", ")} (1 puts every bot on ws)`,
    };
  }
  return { kind: "ok", value };
}

function confoundsGeometry(k: number): boolean {
  return k > 1 && (HD_STRIDE % k === 0 || k % HD_STRIDE === 0);
}

function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

const BALANCED_WS_STRIDES = Array.from({ length: 12 }, (_, i) => i + 2).filter(
  (k) => gcd(k, HD_STRIDE) === 1,
);

/** `null` for a stride coprime to 6; otherwise the HD share of the WebSocket group against the fleet's. */
export function strideHdSkew(wsEvery: number, fleetSize: number): string | null {
  if (gcd(wsEvery, HD_STRIDE) === 1) return null;
  const ws = websocketIndices(wsEvery, fleetSize);
  const all = Array.from({ length: fleetSize }, (_, i) => i);
  const hd = (xs: number[]): number =>
    xs.filter((i) => sourceGeometryForIndex(i) === HD_SOURCE).length;
  return `${TRANSPORT_WS_EVERY_ENV}=${wsEvery} puts 1280x720 bots at ${hd(ws)} of ${ws.length} websocket bots against ${hd(all)} of ${fleetSize} in the fleet; a stride coprime to ${HD_STRIDE} tends to the fleet's 1/${HD_STRIDE} share as the fleet grows`;
}

/** Greppable capture receipt (#2236) — capture geometry, never a published rung. */
export function captureGeometryToken(label: string, source: SourceGeometry): string {
  return taggedLine(label, `captures ${source.width}x${source.height} (issue #2236)`);
}

export type BotIndexResult = { kind: "ok"; value: number } | { kind: "invalid"; message: string };

/** Validates the whole token: `parseInt` would truncate "3junk" to 3. */
export function resolveBaseBotIndex(
  flag: string | undefined,
  env: string | undefined,
): BotIndexResult {
  const raw = flag?.trim() || env;
  if (raw === undefined || raw.trim() === "") return { kind: "ok", value: 0 };
  const token = raw.trim();
  if (!/^\d+$/.test(token) || !Number.isSafeInteger(Number.parseInt(token, 10))) {
    return {
      kind: "invalid",
      message: `--bot-index (or BOT_INDEX) must be a non-negative integer, got "${raw}"`,
    };
  }
  return { kind: "ok", value: Number.parseInt(token, 10) };
}

export function resolveFleetSize(
  flag: string | undefined,
  env: string | undefined,
  fallback: number,
): BotIndexResult {
  const raw = flag?.trim() || env;
  if (raw === undefined || raw.trim() === "") return { kind: "ok", value: fallback };
  const token = raw.trim();
  const value = Number.parseInt(token, 10);
  if (!/^\d+$/.test(token) || !Number.isSafeInteger(value) || value < 1) {
    return {
      kind: "invalid",
      message: `--fleet-size (or BOT_FLEET_SIZE) must be a positive integer, got "${raw}"`,
    };
  }
  return { kind: "ok", value };
}
