import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { type Manifest, type ManifestLine, lineAudioPath, linesForParticipant } from "./manifest";

/**
 * Absolute path to the stitched WAV that
 * {@link prepareParticipantAudio} produces (or has already produced) for a
 * given participant.
 */
export function stitchedAudioPath(outputDir: string, participant: string): string {
  return join(outputDir, `${participant}.wav`);
}

/**
 * Returns true iff the stitched output exists AND is newer than every
 * line WAV the manifest says contributes to it. Used to skip ffmpeg work
 * on repeated invocations.
 */
function isStitchedOutputFresh(outputPath: string, inputPaths: readonly string[]): boolean {
  if (!existsSync(outputPath)) return false;
  const outputMtime = statSync(outputPath).mtimeMs;
  for (const input of inputPaths) {
    if (!existsSync(input)) return false;
    if (statSync(input).mtimeMs > outputMtime) return false;
  }
  return true;
}

/**
 * Build the ffmpeg `concat` demuxer list file that joins one participant's
 * lines (and optionally inserts silence between them, matching the
 * `pause_ms` field on the manifest). Returns the path to the temp list.
 */
function writeConcatList(
  manifestDir: string,
  lines: readonly ManifestLine[],
  pauseMs: number,
  silencePath: string | null,
): string {
  const tmpPath = join(tmpdir(), `bots-app-concat-${process.pid}-${Date.now()}.txt`);
  const body: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const linePath = lineAudioPath(manifestDir, lines[i]);
    body.push(`file '${linePath.replace(/'/g, "'\\''")}'`);
    if (silencePath !== null && pauseMs > 0 && i < lines.length - 1) {
      body.push(`file '${silencePath.replace(/'/g, "'\\''")}'`);
    }
  }
  writeFileSync(tmpPath, body.join("\n") + "\n", "utf8");
  return tmpPath;
}

/**
 * One-shot generate a `pause_ms` silence WAV (matching the line WAV
 * encoding: 48kHz, mono, PCM 16-bit). Cached in `outputDir`. Required so
 * the concat-demuxer pipeline can interleave silence between lines without
 * re-encoding.
 */
function preparePauseSilence(outputDir: string, pauseMs: number): string {
  const path = join(outputDir, `_silence_${pauseMs}ms.wav`);
  if (existsSync(path)) return path;
  const seconds = pauseMs / 1000;
  const ff = spawnSync(
    "ffmpeg",
    [
      "-y",
      "-hide_banner",
      "-loglevel",
      "error",
      "-f",
      "lavfi",
      "-i",
      `anullsrc=cl=mono:r=48000`,
      "-t",
      seconds.toString(),
      "-c:a",
      "pcm_s16le",
      path,
    ],
    { stdio: "inherit" },
  );
  if (ff.status !== 0) {
    throw new Error(`ffmpeg failed to render the ${pauseMs}ms silence padding`);
  }
  return path;
}

export interface PrepareAudioResult {
  /** Absolute path to the stitched per-participant WAV. */
  path: string;
  /** True when ffmpeg ran; false when the cached output was reused. */
  rebuilt: boolean;
  /** Number of source lines that contributed to the stitched output. */
  lineCount: number;
}

/**
 * Stitch one participant's lines into a single WAV via ffmpeg's `concat`
 * demuxer. Idempotent: when every input mtime is older than the existing
 * output, the function returns `rebuilt: false` without spawning ffmpeg.
 *
 * Returns a result whose `lineCount === 0` when the participant has no
 * lines (e.g. a voiceless slot like the `observer-NN` manifest entries).
 * In that case no file is produced and `path` is the not-on-disk location
 * the caller would otherwise have read.
 */
export function prepareParticipantAudio(
  manifest: Manifest,
  manifestDir: string,
  participant: string,
  outputDir: string,
): PrepareAudioResult {
  const outputPath = stitchedAudioPath(outputDir, participant);
  const lines = linesForParticipant(manifest, participant);
  if (lines.length === 0) {
    return { path: outputPath, rebuilt: false, lineCount: 0 };
  }

  mkdirSync(outputDir, { recursive: true });
  const inputPaths = lines.map((line) => lineAudioPath(manifestDir, line));
  if (isStitchedOutputFresh(outputPath, inputPaths)) {
    return { path: outputPath, rebuilt: false, lineCount: lines.length };
  }

  const silencePath =
    manifest.pauseMs > 0 ? preparePauseSilence(outputDir, manifest.pauseMs) : null;
  const concatList = writeConcatList(manifestDir, lines, manifest.pauseMs, silencePath);
  const ff = spawnSync(
    "ffmpeg",
    [
      "-y",
      "-hide_banner",
      "-loglevel",
      "error",
      "-f",
      "concat",
      "-safe",
      "0",
      "-i",
      concatList,
      "-c:a",
      "pcm_s16le",
      "-ar",
      "48000",
      "-ac",
      "1",
      outputPath,
    ],
    { stdio: "inherit" },
  );
  if (ff.status !== 0) {
    throw new Error(`ffmpeg concat failed for participant "${participant}"`);
  }
  return { path: outputPath, rebuilt: true, lineCount: lines.length };
}
