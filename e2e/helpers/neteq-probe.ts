import { BrowserContext, Page } from "@playwright/test";

/**
 * Test-only observer for the remote-audio pipeline.
 *
 * Each remote peer gets a NetEq Dedicated Worker. An AudioWorklet pulls 10 ms
 * frames from it over a MessagePort, so the worker's `GetAudio` runs on the
 * audio clock. The worker reports two things back to the main thread:
 *
 *   - `{type: "audioLevel", rms}` every 50 ms, computed only over frames the
 *     worklet actually pulled. `rms > 0` means non-silent audio reached the
 *     playback path. Silence after silence is not re-posted.
 *   - `{type: "stats", json, ...}`, where `json` is the
 *     `neteq::telemetry::NetEqStats` JSON string (snake_case fields). Workers
 *     post it every second while the diagnostics panel is open and every
 *     five seconds otherwise.
 *
 * The probe wraps `window.Worker` before the app loads and records those
 * messages with wall-clock timestamps, without changing the app's own
 * message handling (it uses a second `message` listener).
 *
 * With `fullRateStats`, the probe also rewrites the app's `setDiagnostics`
 * messages to `enabled: true`, so stats arrive every second without opening
 * the panel. Leave it off to observe the app's own stats rate.
 */

export interface LevelSample {
  t: number;
  worker: number;
  rms: number;
}

export interface StatsSample {
  t: number;
  worker: number;
  getAudioPerSec: number;
  bufferMs: number;
  packetsPerSec: number;
  concealmentEvents: number;
}

interface ProbeState {
  workers: number;
  levels: LevelSample[];
  stats: StatsSample[];
}

export interface ProbeOptions {
  /** Force every NetEq worker to post stats every second. */
  fullRateStats?: boolean;
}

declare global {
  interface Window {
    __neteqProbe?: ProbeState;
  }
}

function installProbe(options: ProbeOptions): void {
  const probe: ProbeState = { workers: 0, levels: [], stats: [] };
  window.__neteqProbe = probe;
  const NativeWorker = window.Worker;

  class ProbedWorker extends NativeWorker {
    constructor(url: string | URL, options?: WorkerOptions) {
      super(url, options);
      const id = probe.workers++;
      this.addEventListener("message", (event: MessageEvent) => {
        const data = event.data;
        if (!data || typeof data !== "object") {
          return;
        }
        if (data.type === "audioLevel" && typeof data.rms === "number") {
          probe.levels.push({ t: Date.now(), worker: id, rms: data.rms });
        } else if (data.type === "stats" && typeof data.json === "string") {
          const s = JSON.parse(data.json);
          const ops = s.network?.operation_counters ?? {};
          // Every GetAudio runs exactly one operation, so the per-second
          // operation counts add up to GetAudio calls per second.
          const getAudioPerSec = [
            "normal_per_sec",
            "expand_per_sec",
            "accelerate_per_sec",
            "fast_accelerate_per_sec",
            "preemptive_expand_per_sec",
            "merge_per_sec",
            "comfort_noise_per_sec",
            "dtmf_per_sec",
            "undefined_per_sec",
          ].reduce((sum, key) => sum + (Number(ops[key]) || 0), 0);
          probe.stats.push({
            t: Date.now(),
            worker: id,
            getAudioPerSec,
            bufferMs: Number(s.current_buffer_size_ms) || 0,
            packetsPerSec: Number(s.packets_per_sec) || 0,
            concealmentEvents: Number(s.lifetime?.concealment_events) || 0,
          });
        }
      });
    }

    postMessage(message: unknown, transfer?: Transferable[] | StructuredSerializeOptions): void {
      const command = message as { cmd?: unknown } | null;
      if (options.fullRateStats && command && command.cmd === "setDiagnostics") {
        message = { cmd: "setDiagnostics", enabled: true };
      }
      if (Array.isArray(transfer)) {
        super.postMessage(message, transfer);
      } else {
        super.postMessage(message, transfer);
      }
    }
  }

  window.Worker = ProbedWorker;
}

/** Install the probe in every page of `context`, before any app script runs. */
export async function installNetEqProbe(
  context: BrowserContext,
  options: ProbeOptions = {},
): Promise<void> {
  await context.addInitScript(installProbe, options);
}

export async function audioLevelsSince(page: Page, since: number): Promise<LevelSample[]> {
  return page.evaluate((t) => (window.__neteqProbe?.levels ?? []).filter((l) => l.t >= t), since);
}

export async function statsSince(page: Page, since: number): Promise<StatsSample[]> {
  return page.evaluate((t) => (window.__neteqProbe?.stats ?? []).filter((s) => s.t >= t), since);
}

/** Wall-clock time of the first non-silent frame reported after `since`, or null. */
export async function firstAudibleSince(page: Page, since: number): Promise<number | null> {
  const levels = await audioLevelsSince(page, since);
  const hit = levels.find((l) => l.rms > 0);
  return hit ? hit.t : null;
}
