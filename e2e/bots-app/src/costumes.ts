import { spawnSync } from "node:child_process";
import { closeSync, existsSync, mkdirSync, openSync, readSync, statSync } from "node:fs";
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

/**
 * Whether a cached y4m was encoded at the current target geometry. The header is
 * a plain-ASCII first line (`YUV4MPEG2 W640 H480 F30:1 ...`), so this reads a few
 * bytes rather than the whole file.
 */
export function y4mMatchesTargetGeometry(path: string): boolean {
  let fd: number | undefined;
  try {
    fd = openSync(path, "r");
    const buf = Buffer.alloc(64);
    const n = readSync(fd, buf, 0, buf.length, 0);
    // Bound to the first line and require the magic: without it any 64-byte
    // prefix containing `W640`/`H480` passes, including a non-y4m file.
    const firstLine = buf.subarray(0, n).toString("ascii").split("\n")[0];
    if (!firstLine.startsWith("YUV4MPEG2")) return false;
    return (
      new RegExp(`\\bW${COSTUME_WIDTH}\\b`).test(firstLine) &&
      new RegExp(`\\bH${COSTUME_HEIGHT}\\b`).test(firstLine)
    );
  } catch {
    // Unreadable or truncated: treat as a miss so it is rebuilt.
    return false;
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}

/**
 * Costume source geometry — the size a real webcam publishes (#2171).
 *
 * The ffmpeg chain scales to cover, centre-crops, and forces square pixels.
 * `scale` alone keeps the source's DISPLAY aspect by declaring a non-square SAR,
 * and Chrome passes the raster through unchanged — so the bot would publish a
 * horizontally squashed face at the right pixel count.
 *
 * `force_original_aspect_ratio=increase` + `crop` rather than a bare
 * `crop=ih*4/3:ih`: the latter computes an output width from the height and
 * `vf_crop` REJECTS `out_w > in_w`, so it hard-fails on any source narrower than
 * 4:3. The costume MP4s are third-party recordings with no guaranteed aspect
 * (`bot/README.md`), and the old `scale=1280:720` force-stretched anything and
 * never failed — so this form must not introduce a new failure mode.
 */
export const COSTUME_WIDTH = 640;
export const COSTUME_HEIGHT = 480;

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
  // The mtime check alone cannot see a GEOMETRY change: the source MP4 is
  // untouched when the target size changes, so a stale 720p y4m would be reused
  // and #2171 would ship inert. The header check is what makes the fix land.
  if (
    existsSync(outputPath) &&
    statSync(outputPath).mtimeMs >= statSync(sourcePath).mtimeMs &&
    y4mMatchesTargetGeometry(outputPath)
  ) {
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
      `scale=${COSTUME_WIDTH}:${COSTUME_HEIGHT}:force_original_aspect_ratio=increase,` +
        `crop=${COSTUME_WIDTH}:${COSTUME_HEIGHT},setsar=1,fps=30`,
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
