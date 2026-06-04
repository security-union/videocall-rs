import { existsSync, statSync } from "node:fs";
import { join } from "node:path";

import { costumeY4mPath, prepareParticipantCostume } from "./costumes";
import {
  type Manifest,
  costumeNameForParticipant,
  lineAudioPath,
  linesForParticipant,
} from "./manifest";
import { prepareParticipantAudio, stitchedAudioPath } from "./stitcher";

/**
 * Step of the auto-prime pipeline currently in flight for a participant.
 * The dashboard surfaces each event as a single log line; operators see
 * "Priming assets" briefly in the Running Bots table while the helper
 * works, then the bot transitions to the existing `launching` →
 * `joining` → `in-meeting` flow.
 *
 * Variants:
 *   - `checking`        — looking at the on-disk state to decide what work to do.
 *   - `priming-audio`   — ffmpeg is stitching the participant's WAV.
 *   - `priming-costume` — ffmpeg is converting the costume MP4 → y4m.
 *   - `done`            — pipeline finished (some primed, some skipped).
 *   - `skipped`         — the cached output was already up-to-date.
 *   - `failed`          — one of the prepare helpers threw; we swallow
 *                          the error so the bot launch can continue with
 *                          Chrome's default fake devices, but we surface
 *                          the failure in the message for the log dialog.
 */
export interface PrimeProgress {
  participant: string;
  step: "checking" | "priming-audio" | "priming-costume" | "done" | "skipped" | "failed";
  message: string;
  /** Wall-clock duration of the step in milliseconds. Set on `done` / `failed`. */
  durationMs?: number;
}

/**
 * Outcome of {@link ensureAssetsPrimed}. Each pair of booleans tells the
 * caller whether the helper produced fresh output (`*Primed: true`) or
 * left the existing cached output untouched (`skipped.*: true`). Both
 * false for a kind means the participant has no source for that kind in
 * the manifest (no lines → no audio; no costume_dir → no costume) — the
 * bot launch will then fall back to Chrome's default fake pattern for
 * that media.
 */
export interface PrimeResult {
  audioPrimed: boolean;
  costumePrimed: boolean;
  skipped: { audio: boolean; costume: boolean };
}

/**
 * Tiny abstraction over the prepare helpers so the unit tests can
 * substitute fake implementations without touching the filesystem or
 * spawning ffmpeg. The defaults wire through to the real helpers in
 * `./stitcher` + `./costumes` — production callers never set `deps`.
 */
export interface PrimeDeps {
  prepareAudio?: typeof prepareParticipantAudio;
  prepareCostume?: typeof prepareParticipantCostume;
}

/**
 * Ensure both per-participant fake-device files are present and
 * up-to-date before {@link launchBot} hands them to Chrome via
 * `--use-file-for-fake-{audio,video}-capture`. Skips work when the
 * cached output is already newer than every input; runs the matching
 * `prepare*` helper otherwise.
 *
 * Errors are caught and surfaced via the `failed` progress event so a
 * missing source file (typo'd participant, torn manifest, costume zip
 * not unpacked, ...) cannot prevent the bot from launching — the
 * caller falls back to Chrome's default fake pattern for the affected
 * kind.
 *
 * SSH-hosted bots should NOT call this — the assets they consume live
 * on the remote host, not on the orchestrator's local filesystem.
 * Skipping is the orchestrator's responsibility (the SSH launch flow
 * bypasses `launchBot` entirely, so `ensureAssetsPrimed` is never
 * reached for SSH bots in practice).
 *
 * `manifestDir` is the directory the manifest's `audio_file` paths are
 * anchored against (i.e. the value `loadManifest` returns alongside
 * the parsed manifest). Without it, the helper cannot resolve line
 * WAV paths for the freshness check; callers that don't have a
 * manifestDir on hand should not invoke ensureAssetsPrimed and let
 * the bot fall through to default fake devices.
 *
 * `costumeSource` defaults to `<repoRoot>/bot/assets/costumes` (the
 * same path the CLI's `prep-assets` command uses), resolved relative
 * to this module. Callers that put the unpacked costume zip elsewhere
 * (preview environments, CI, ...) pass the override.
 */
export async function ensureAssetsPrimed(args: {
  manifest: Manifest;
  manifestDir: string;
  runDir: string;
  participant: string;
  costumeSource?: string;
  onProgress?: (p: PrimeProgress) => void;
  deps?: PrimeDeps;
}): Promise<PrimeResult> {
  const { manifest, manifestDir, runDir, participant } = args;
  const emit = args.onProgress ?? noop;
  const prepareAudio = args.deps?.prepareAudio ?? prepareParticipantAudio;
  const prepareCostume = args.deps?.prepareCostume ?? prepareParticipantCostume;

  // Unknown participant — short-circuit. The bot launch will fall back
  // to Chrome's default fake devices, which is the same behaviour the
  // pre-auto-prime CLI produced when an operator typo'd `--participant`.
  const inManifest = manifest.participants.some((p) => p.name === participant);
  if (!inManifest) {
    emit({
      participant,
      step: "skipped",
      message: `participant '${participant}' is not in the manifest — falling back to Chrome's default fake devices`,
    });
    return {
      audioPrimed: false,
      costumePrimed: false,
      skipped: { audio: true, costume: true },
    };
  }

  emit({
    participant,
    step: "checking",
    message: "checking whether stitched WAV + costume y4m are up-to-date",
  });

  const audioDir = join(runDir, "audio");
  const costumesDir = join(runDir, "costumes");

  // -------- Audio --------
  const lines = linesForParticipant(manifest, participant);
  let audioPrimed = false;
  let audioSkipped = false;
  if (lines.length === 0) {
    // Voiceless slot (e.g. `tina` / `observer-NN`) — nothing to stitch.
    audioSkipped = true;
  } else {
    const wavPath = stitchedAudioPath(audioDir, participant);
    if (isAudioFresh(wavPath, manifest, manifestDir, participant)) {
      audioSkipped = true;
      emit({
        participant,
        step: "skipped",
        message: `audio cached at ${wavPath}`,
      });
    } else {
      const t0 = Date.now();
      emit({
        participant,
        step: "priming-audio",
        message: `stitching ${lines.length} line(s) → ${wavPath}`,
      });
      try {
        const result = prepareAudio(manifest, manifestDir, participant, audioDir);
        if (result.rebuilt) {
          audioPrimed = true;
        } else {
          audioSkipped = true;
        }
      } catch (e) {
        emit({
          participant,
          step: "failed",
          message: `audio prep failed: ${(e as Error).message} — falling back to default fake mic`,
          durationMs: Date.now() - t0,
        });
        audioSkipped = true;
      }
    }
  }

  // -------- Costume --------
  const costumeName = costumeNameForParticipant(manifest, participant);
  let costumePrimed = false;
  let costumeSkipped = false;
  if (costumeName === undefined) {
    // Participant has no `costume_dir` in the manifest — the bot will
    // fall back to Chrome's default fake camera. Nothing to do.
    costumeSkipped = true;
  } else {
    const costumeOut = costumeY4mPath(costumesDir, manifest, participant);
    const costumeSourceDir = args.costumeSource ?? defaultCostumeSource();
    const sourceMp4 = join(costumeSourceDir, costumeName, "talking.mp4");
    if (costumeOut !== null && isCostumeFresh(costumeOut, sourceMp4)) {
      costumeSkipped = true;
      emit({
        participant,
        step: "skipped",
        message: `costume cached at ${costumeOut}`,
      });
    } else {
      const t0 = Date.now();
      emit({
        participant,
        step: "priming-costume",
        message: `converting ${sourceMp4} → ${costumeOut ?? "<unknown>"}`,
      });
      try {
        const result = prepareCostume(manifest, participant, costumeSourceDir, costumesDir);
        if (result.rebuilt) {
          costumePrimed = true;
        } else {
          costumeSkipped = true;
        }
      } catch (e) {
        emit({
          participant,
          step: "failed",
          message: `costume prep failed: ${(e as Error).message} — falling back to default fake camera`,
          durationMs: Date.now() - t0,
        });
        costumeSkipped = true;
      }
    }
  }

  emit({
    participant,
    step: "done",
    message: `prime complete (audio ${audioPrimed ? "primed" : "skipped"}, costume ${costumePrimed ? "primed" : "skipped"})`,
  });

  return {
    audioPrimed,
    costumePrimed,
    skipped: { audio: audioSkipped, costume: costumeSkipped },
  };
}

/**
 * Default costume-source directory: `<repoRoot>/bot/assets/costumes`,
 * resolved relative to this file. Mirrors the CLI's `--costume-source`
 * default in `cli.ts`'s `prep-assets` command.
 */
function defaultCostumeSource(): string {
  const here = new URL(".", import.meta.url).pathname;
  // src → bots-app → e2e → repoRoot, then `bot/assets/costumes`.
  return join(here, "..", "..", "..", "bot", "assets", "costumes");
}

/**
 * True iff the stitched WAV exists AND its mtime is newer than every
 * line WAV the manifest says contributes to it. Mirrors the same check
 * `stitcher.ts` performs internally; we duplicate it here so the
 * helper can decide "skip vs. prime" without calling the prepare
 * function (which is the unit test contract — `prepareAudio` is
 * mocked).
 */
function isAudioFresh(
  wavPath: string,
  manifest: Manifest,
  manifestDir: string,
  participant: string,
): boolean {
  if (!existsSync(wavPath)) return false;
  const outMtime = statSync(wavPath).mtimeMs;
  const lines = linesForParticipant(manifest, participant);
  for (const line of lines) {
    const linePath = lineAudioPath(manifestDir, line);
    if (!existsSync(linePath)) {
      // Source missing — we can't compare mtimes. Treat the cached
      // output as fresh enough; the prepare helper will throw if it
      // really can't proceed, and the auto-prime catches that.
      continue;
    }
    if (statSync(linePath).mtimeMs > outMtime) return false;
  }
  return true;
}

/**
 * True iff the costume y4m exists AND its mtime is newer than (or
 * equal to) the source MP4's mtime. Mirrors the equivalent check in
 * `costumes.ts::prepareParticipantCostume`.
 */
function isCostumeFresh(y4mPath: string, sourceMp4: string): boolean {
  if (!existsSync(y4mPath)) return false;
  if (!existsSync(sourceMp4)) {
    // Source missing — the prepare helper will throw and the
    // auto-prime catches that. Treat the cached output as fresh here so
    // we don't spuriously try to rebuild it and then fail loudly.
    return true;
  }
  return statSync(y4mPath).mtimeMs >= statSync(sourceMp4).mtimeMs;
}

function noop(_p: PrimeProgress): void {
  // Intentionally empty — used as the default `onProgress` so callers
  // can omit it without us guarding every emit() with `if (cb)`.
}
