import { HD_SOURCE, type SourceGeometry } from "./posture";

export const VIDEO_NOISE_ENV = "BOT_VIDEO_NOISE";

/** Noise alpha over the clock background, per source geometry (issue 3089). */
export const SD_VIDEO_NOISE = 0.1;
export const HD_VIDEO_NOISE = 0.15;

export function videoNoiseFor(source: SourceGeometry): number {
  return source.width === HD_SOURCE.width && source.height === HD_SOURCE.height
    ? HD_VIDEO_NOISE
    : SD_VIDEO_NOISE;
}

/** The override wins; unset means the source geometry's default. */
export function effectiveVideoNoise(opts: {
  videoNoise?: number | null;
  sourceGeometry: SourceGeometry;
}): number {
  return opts.videoNoise ?? videoNoiseFor(opts.sourceGeometry);
}

/** Greppable per-bot receipt of the alpha the clock source was handed. */
export function videoNoiseLine(opts: {
  videoNoise?: number | null;
  sourceGeometry: SourceGeometry;
}): string {
  const { width, height } = opts.sourceGeometry;
  const fallback = videoNoiseFor(opts.sourceGeometry);
  const origin =
    opts.videoNoise == null
      ? `default for ${width}x${height}`
      : `override; default for ${width}x${height} is ${fallback}`;
  return `video noise: alpha ${effectiveVideoNoise(opts)} (${origin})`;
}

export type VideoNoiseResult =
  | { kind: "ok"; value: number | undefined }
  | { kind: "invalid"; message: string };

/** `--video-noise` / `BOT_VIDEO_NOISE`: unset or empty ⇒ per-geometry default; else a decimal in [0, 1]. */
export function resolveVideoNoise(raw: string | undefined): VideoNoiseResult {
  const token = raw?.trim() ?? "";
  if (token === "") return { kind: "ok", value: undefined };
  const value = Number(token);
  if (!/^(?:\d+(?:\.\d+)?|\.\d+)$/.test(token) || value > 1) {
    return {
      kind: "invalid",
      message: `--video-noise (or ${VIDEO_NOISE_ENV}) must be a number from 0 to 1, got "${raw}"`,
    };
  }
  return { kind: "ok", value };
}
