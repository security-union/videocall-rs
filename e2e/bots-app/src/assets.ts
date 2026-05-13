import { existsSync } from "node:fs";
import { join } from "node:path";

import { costumeY4mPath } from "./costumes";
import { type Manifest } from "./manifest";
import { stitchedAudioPath } from "./stitcher";

export interface ResolvedAssets {
  /** Absolute path to the per-participant stitched WAV, or `null` when prep-assets has not produced one. */
  audioPath: string | null;
  /** Absolute path to the costume y4m, or `null` when the participant has no costume_dir OR prep-assets has not produced it yet. */
  videoPath: string | null;
}

/**
 * Look up the prep'd fake-device files for one participant.
 *
 * The returned paths are absolute (so they can be passed to Chrome's
 * `--use-file-for-fake-*-capture` flags as-is). Missing files become
 * `null` rather than throwing — the caller decides whether a missing
 * asset is fatal or a graceful "fall back to Chrome's default fake
 * pattern" case.
 *
 * `runDir` is the directory that {@link stitchedAudioPath} and
 * {@link costumeY4mPath} write into — i.e. `<output-dir>` from
 * `bots-app prep-assets`, which defaults to `e2e/bots-app/run`. The two
 * sub-dirs (`audio/` and `costumes/`) are produced under it.
 */
export function resolveAssetsForParticipant(args: {
  manifest: Manifest;
  runDir: string;
  participant: string;
}): ResolvedAssets {
  const audioDir = join(args.runDir, "audio");
  const costumesDir = join(args.runDir, "costumes");

  const audioCandidate = stitchedAudioPath(audioDir, args.participant);
  const audioPath = existsSync(audioCandidate) ? audioCandidate : null;

  const videoCandidate = costumeY4mPath(costumesDir, args.manifest, args.participant);
  const videoPath = videoCandidate !== null && existsSync(videoCandidate) ? videoCandidate : null;

  return { audioPath, videoPath };
}
