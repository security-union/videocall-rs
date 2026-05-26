import { existsSync } from "node:fs";
import { join } from "node:path";

import { randomUUID } from "node:crypto";

import { prepareParticipantCostume } from "../costumes";
import { loadManifest } from "../manifest";
import { prepareParticipantAudio } from "../stitcher";

/**
 * One in-flight (or recently-finished) prep-assets job. The control
 * surface keeps a `Map<jobId, PrepAssetsJob>` while running and for a
 * grace period after completion so the dashboard can fetch final logs
 * via `GET /api/assets/prep/:jobId` after the SSE stream disconnects.
 */
export interface PrepAssetsJob {
  jobId: string;
  status: "running" | "done" | "failed";
  startedAt: number;
  finishedAt?: number;
  stdoutLog: string[];
  exitCode: number | null;
  error?: string;
  /** Summary counts populated when the job completes. */
  audioPrepped: number;
  costumesPrepped: number;
  /**
   * Subscribers that want a live feed. Each callback fires for every
   * appended log line and once for the terminal `done`/`failed`
   * transition (with `null`). Removed via the `unsubscribe` returned
   * by `subscribe()`.
   */
  subscribers: Set<(line: string | null) => void>;
}

export interface PrepAssetsOptions {
  manifestPath: string;
  costumeSource: string;
  outputDir: string;
  participants?: string[];
}

/**
 * How long a finished job stays in the registry before the sweeper
 * drops it. 30 minutes gives the dashboard's "view recent logs" panel
 * enough headroom for an operator who alt-tabbed away.
 */
export const PREP_ASSETS_RETENTION_MS = 30 * 60 * 1000;

/**
 * Append a line to the job's log buffer AND notify every subscriber.
 * Exposed for the route handler's SSE wiring + the in-process tests.
 */
export function emitLine(job: PrepAssetsJob, line: string): void {
  job.stdoutLog.push(line);
  for (const sub of job.subscribers) {
    try {
      sub(line);
    } catch {
      // Subscriber threw — drop it on the floor; the job's own
      // forward progress must not be blocked by a misbehaving SSE
      // listener.
    }
  }
}

/**
 * Final notification: emit a `null` line so SSE consumers know to
 * close, then drop every subscriber. Called once when the job
 * transitions out of `"running"`.
 */
function finalize(job: PrepAssetsJob): void {
  for (const sub of job.subscribers) {
    try {
      sub(null);
    } catch {
      // ignore
    }
  }
  job.subscribers.clear();
}

/**
 * In-process implementation of `bots-app prep-assets`. Runs in the
 * background (the caller does NOT await it); progress flows to the
 * job's `stdoutLog` + subscribers. The dashboard's SSE endpoint
 * forwards `subscribers` events to the browser.
 *
 * Errors per-participant are logged into the buffer but do NOT fail
 * the whole job — the CLI's prep-assets has the same forgiving
 * behavior (`console.error` per failure, then continue). Only an
 * up-front error (missing manifest / bad YAML) fails the job.
 */
export async function runPrepAssetsJob(job: PrepAssetsJob, opts: PrepAssetsOptions): Promise<void> {
  try {
    if (!existsSync(opts.manifestPath)) {
      throw new Error(
        `manifest not found at ${opts.manifestPath} — run \`python3 bot/generate-conversation-edge.py\` first`,
      );
    }
    const { manifest, manifestDir } = loadManifest(opts.manifestPath);
    const audioDir = join(opts.outputDir, "audio");
    const costumesOutDir = join(opts.outputDir, "costumes");
    const requested =
      opts.participants && opts.participants.length > 0
        ? opts.participants
        : manifest.participants.map((p) => p.name);
    emitLine(
      job,
      `prep-assets: starting (${requested.length} participant(s), manifest=${opts.manifestPath})`,
    );
    let audioPrepped = 0;
    let costumesPrepped = 0;
    for (const participant of requested) {
      try {
        const audio = prepareParticipantAudio(manifest, manifestDir, participant, audioDir);
        if (audio.lineCount > 0) {
          audioPrepped += 1;
          emitLine(
            job,
            `[${participant}] audio ${audio.rebuilt ? "stitched" : "cached"} (${audio.lineCount} lines) -> ${audio.path}`,
          );
        }
        if (!existsSync(opts.costumeSource)) {
          emitLine(
            job,
            `prep-assets: costume source ${opts.costumeSource} not found — skipping y4m conversion`,
          );
          continue;
        }
        const costume = prepareParticipantCostume(
          manifest,
          participant,
          opts.costumeSource,
          costumesOutDir,
        );
        if (costume.path !== null) {
          costumesPrepped += 1;
          emitLine(
            job,
            `[${participant}] costume ${costume.rebuilt ? "converted" : "cached"} (${costume.costumeName}) -> ${costume.path}`,
          );
        }
      } catch (e) {
        emitLine(job, `[${participant}] prep failed: ${(e as Error).message}`);
      }
    }
    emitLine(
      job,
      `prep-assets done — ${audioPrepped} audio file(s), ${costumesPrepped} costume(s)`,
    );
    job.audioPrepped = audioPrepped;
    job.costumesPrepped = costumesPrepped;
    job.status = "done";
    job.exitCode = 0;
  } catch (e) {
    emitLine(job, `prep-assets failed: ${(e as Error).message}`);
    job.status = "failed";
    job.error = (e as Error).message;
    job.exitCode = 1;
  } finally {
    job.finishedAt = Date.now();
    finalize(job);
  }
}

/**
 * Create a fresh job record (no work started yet). Caller wires up the
 * background promise from {@link runPrepAssetsJob}.
 */
export function createPrepAssetsJob(): PrepAssetsJob {
  return {
    jobId: randomUUID(),
    status: "running",
    startedAt: Date.now(),
    stdoutLog: [],
    exitCode: null,
    audioPrepped: 0,
    costumesPrepped: 0,
    subscribers: new Set(),
  };
}

/**
 * Filename validation for the optional override fields. Defensive
 * even though the dashboard's form only ever sends fixed defaults —
 * the API is exposed and operators may hit it from `curl`.
 */
export const PREP_ASSETS_PATH_PATTERN = /^[A-Za-z0-9_./@+-]+$/;

/**
 * Validate (and normalise) one of the override paths. Rejects absolute
 * paths, `..` segments, and any character outside the whitelist. The
 * caller decides whether `undefined` is acceptable.
 */
export function validatePrepAssetsPath(value: unknown, field: string): string {
  if (typeof value !== "string" || value === "") {
    throw new Error(`"${field}" must be a non-empty string`);
  }
  if (!PREP_ASSETS_PATH_PATTERN.test(value)) {
    throw new Error(`"${field}" contains invalid characters; allowed: A-Z a-z 0-9 _ . / @ + -`);
  }
  if (value.startsWith("/") || value.includes("..")) {
    throw new Error(`"${field}" must be a relative path without "..": got "${value}"`);
  }
  return value;
}

/**
 * Drop completed jobs older than {@link PREP_ASSETS_RETENTION_MS}.
 * Called inline before serving any GET /assets/prep/* response so we
 * never need a setInterval keeping the event loop awake.
 */
export function sweepStalePrepAssetsJobs(
  jobs: Map<string, PrepAssetsJob>,
  now: number = Date.now(),
): void {
  for (const [id, job] of jobs) {
    if (
      job.status !== "running" &&
      job.finishedAt !== undefined &&
      now - job.finishedAt > PREP_ASSETS_RETENTION_MS
    ) {
      jobs.delete(id);
    }
  }
}
