import { taggedLine } from "./log-line";

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
