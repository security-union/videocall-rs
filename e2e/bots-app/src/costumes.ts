import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, statSync } from "node:fs";
import { join } from "node:path";

import { type Manifest, costumeNameForParticipant } from "./manifest";

/**
 * Absolute path to the y4m file that {@link prepareParticipantCostume}
 * produces (or has already produced) for a given participant. Returns
 * `null` when the manifest doesn't assign that participant a costume.
 */
export function costumeY4mPath(
  outputDir: string,
  manifest: Manifest,
  participant: string,
): string | null {
  const name = costumeNameForParticipant(manifest, participant);
  if (!name) return null;
  return join(outputDir, `${name}.y4m`);
}

export interface PrepareCostumeResult {
  /** Absolute path to the y4m file, or `null` if the participant has no costume. */
  path: string | null;
  /** True when ffmpeg ran; false when the cached output was reused. */
  rebuilt: boolean;
  /** Costume name (basename of `costume_dir`) or `null` for voiceless slots. */
  costumeName: string | null;
}

/**
 * Convert one participant's costume MP4 (the talking variant) to y4m so
 * Chrome's `--use-file-for-fake-video-capture=<path>` can consume it.
 * Idempotent: when the output is newer than the source the function
 * returns `rebuilt: false` without spawning ffmpeg.
 *
 * The Rust bot ships costumes as raw I420 frames after a one-shot ffmpeg
 * pass (see `bot/README.md`). Browser Chrome wants y4m specifically, so
 * the conversion source is the original `talking.mp4` next to the I420
 * cache, NOT the I420 itself (which loses container metadata Chrome
 * needs to identify resolution + framerate).
 *
 * `costumeSourceDir` is the directory that contains per-costume folders
 * with a `talking.mp4` inside, e.g. `/tmp/costume-videos` (after the
 * release-zip unpack) or `bot/assets/costumes` (after the I420 step has
 * been run by the user — same folder structure as the upstream zip).
 */
export function prepareParticipantCostume(
  manifest: Manifest,
  participant: string,
  costumeSourceDir: string,
  outputDir: string,
): PrepareCostumeResult {
  const costumeName = costumeNameForParticipant(manifest, participant);
  if (!costumeName) {
    return { path: null, rebuilt: false, costumeName: null };
  }

  const outputPath = join(outputDir, `${costumeName}.y4m`);
  const sourcePath = join(costumeSourceDir, costumeName, "talking.mp4");

  if (!existsSync(sourcePath)) {
    throw new Error(
      `costume source MP4 not found at ${sourcePath} — unzip costume-videos.zip into ${costumeSourceDir} first`,
    );
  }

  mkdirSync(outputDir, { recursive: true });
  if (existsSync(outputPath) && statSync(outputPath).mtimeMs >= statSync(sourcePath).mtimeMs) {
    return { path: outputPath, rebuilt: false, costumeName };
  }

  const ff = spawnSync(
    "ffmpeg",
    [
      "-y",
      "-hide_banner",
      "-loglevel",
      "error",
      "-i",
      sourcePath,
      "-vf",
      "scale=1280:720,fps=30",
      "-pix_fmt",
      "yuv420p",
      "-f",
      "yuv4mpegpipe",
      outputPath,
    ],
    { stdio: "inherit" },
  );
  if (ff.status !== 0) {
    throw new Error(`ffmpeg y4m conversion failed for costume "${costumeName}"`);
  }
  return { path: outputPath, rebuilt: true, costumeName };
}
