import { mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ensureAssetsPrimed, type PrimeProgress } from "./auto-prime";
import { parseManifestText } from "./manifest";

const FIXTURE = `
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: bob
  costume_dir: assets/costumes/cat
- name: tina
pause_ms: 0
lines:
- speaker: alice
  audio_file: lines/alice_000.wav
- speaker: bob
  audio_file: lines/bob_000.wav
`;

interface PreparedResultStub {
  path: string;
  rebuilt: boolean;
  lineCount: number;
  costumeName: string | null;
}

/**
 * Helper that builds a fake `prepareParticipantAudio` / `prepareParticipantCostume`
 * pair: the audio fake writes a WAV at the conventional path so the
 * "skipped because cached" check in the helper observes a real file;
 * the costume fake does the same for the y4m. A `throwingAudio` flag
 * propagates from the helper's catch path to the unit test asserting
 * graceful degradation.
 */
function fakes(
  options: {
    throwAudio?: boolean;
    throwCostume?: boolean;
    audioRebuilt?: boolean;
    costumeRebuilt?: boolean;
  } = {},
) {
  const audioCalls: string[] = [];
  const costumeCalls: string[] = [];

  const prepareAudio = (
    _manifest: unknown,
    _manifestDir: string,
    participant: string,
    outputDir: string,
  ): PreparedResultStub => {
    audioCalls.push(participant);
    if (options.throwAudio) {
      throw new Error("fake ffmpeg crash");
    }
    const path = join(outputDir, `${participant}.wav`);
    mkdirSync(outputDir, { recursive: true });
    writeFileSync(path, "RIFF");
    return {
      path,
      rebuilt: options.audioRebuilt ?? true,
      lineCount: 1,
      costumeName: null,
    };
  };

  const prepareCostume = (
    manifest: unknown,
    participant: string,
    _costumeSourceDir: string,
    outputDir: string,
  ): PreparedResultStub => {
    costumeCalls.push(participant);
    if (options.throwCostume) {
      throw new Error("fake costume crash");
    }
    // The real helper derives the costume name from the manifest; for
    // the fake we just look up "pirate" / "cat" by participant.
    const m = manifest as { participants: Array<{ name: string; costumeDir?: string }> };
    const row = m.participants.find((p) => p.name === participant);
    const costumeName = row?.costumeDir?.split("/").pop() ?? null;
    if (costumeName === null) {
      return { path: null as unknown as string, rebuilt: false, lineCount: 0, costumeName: null };
    }
    const path = join(outputDir, `${costumeName}.y4m`);
    mkdirSync(outputDir, { recursive: true });
    writeFileSync(path, "YUV4MPEG2");
    return {
      path,
      rebuilt: options.costumeRebuilt ?? true,
      lineCount: 0,
      costumeName,
    };
  };

  return {
    prepareAudio: prepareAudio as never,
    prepareCostume: prepareCostume as never,
    audioCalls,
    costumeCalls,
  };
}

describe("ensureAssetsPrimed", () => {
  let runDir: string;
  let manifestDir: string;
  let costumeSource: string;

  beforeEach(() => {
    runDir = mkdtempSync(join(tmpdir(), "bots-app-auto-prime-test-run-"));
    manifestDir = mkdtempSync(join(tmpdir(), "bots-app-auto-prime-test-manifest-"));
    costumeSource = mkdtempSync(join(tmpdir(), "bots-app-auto-prime-test-costumes-"));
    // Seed a per-participant line WAV + costume MP4 so the freshness
    // checks have something to mtime-compare against.
    mkdirSync(join(manifestDir, "lines"), { recursive: true });
    writeFileSync(join(manifestDir, "lines", "alice_000.wav"), "RIFF");
    writeFileSync(join(manifestDir, "lines", "bob_000.wav"), "RIFF");
    mkdirSync(join(costumeSource, "pirate"), { recursive: true });
    mkdirSync(join(costumeSource, "cat"), { recursive: true });
    writeFileSync(join(costumeSource, "pirate", "talking.mp4"), "FAKEMP4");
    writeFileSync(join(costumeSource, "cat", "talking.mp4"), "FAKEMP4");
  });

  afterEach(() => {
    rmSync(runDir, { recursive: true, force: true });
    rmSync(manifestDir, { recursive: true, force: true });
    rmSync(costumeSource, { recursive: true, force: true });
  });

  it("returns skipped for both when cached WAV + y4m are up-to-date", async () => {
    const manifest = parseManifestText(FIXTURE);
    // Pre-seed the cached outputs and bump their mtime ahead of the
    // sources so the freshness check returns true.
    mkdirSync(join(runDir, "audio"), { recursive: true });
    mkdirSync(join(runDir, "costumes"), { recursive: true });
    writeFileSync(join(runDir, "audio", "alice.wav"), "RIFF");
    writeFileSync(join(runDir, "costumes", "pirate.y4m"), "YUV4MPEG2");
    const future = new Date(Date.now() + 60_000);
    utimesSync(join(runDir, "audio", "alice.wav"), future, future);
    utimesSync(join(runDir, "costumes", "pirate.y4m"), future, future);
    const f = fakes();
    const events: PrimeProgress[] = [];
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      onProgress: (p) => events.push(p),
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(false);
    expect(result.costumePrimed).toBe(false);
    expect(result.skipped.audio).toBe(true);
    expect(result.skipped.costume).toBe(true);
    expect(f.audioCalls).toEqual([]);
    expect(f.costumeCalls).toEqual([]);
    expect(events.some((e) => e.step === "skipped" && e.message.includes("audio"))).toBe(true);
    expect(events.some((e) => e.step === "skipped" && e.message.includes("costume"))).toBe(true);
    expect(events.at(-1)?.step).toBe("done");
  });

  it("primes only audio when the WAV is missing but the costume is cached", async () => {
    const manifest = parseManifestText(FIXTURE);
    mkdirSync(join(runDir, "costumes"), { recursive: true });
    writeFileSync(join(runDir, "costumes", "pirate.y4m"), "YUV4MPEG2");
    const future = new Date(Date.now() + 60_000);
    utimesSync(join(runDir, "costumes", "pirate.y4m"), future, future);
    const f = fakes();
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(true);
    expect(result.costumePrimed).toBe(false);
    expect(result.skipped.audio).toBe(false);
    expect(result.skipped.costume).toBe(true);
    expect(f.audioCalls).toEqual(["alice"]);
    expect(f.costumeCalls).toEqual([]);
  });

  it("primes only the costume when the WAV is cached and the y4m is missing", async () => {
    const manifest = parseManifestText(FIXTURE);
    mkdirSync(join(runDir, "audio"), { recursive: true });
    writeFileSync(join(runDir, "audio", "alice.wav"), "RIFF");
    const future = new Date(Date.now() + 60_000);
    utimesSync(join(runDir, "audio", "alice.wav"), future, future);
    const f = fakes();
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(false);
    expect(result.costumePrimed).toBe(true);
    expect(result.skipped.audio).toBe(true);
    expect(result.skipped.costume).toBe(false);
    expect(f.audioCalls).toEqual([]);
    expect(f.costumeCalls).toEqual(["alice"]);
  });

  it("primes both when neither output exists", async () => {
    const manifest = parseManifestText(FIXTURE);
    const f = fakes();
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(true);
    expect(result.costumePrimed).toBe(true);
    expect(result.skipped.audio).toBe(false);
    expect(result.skipped.costume).toBe(false);
    expect(f.audioCalls).toEqual(["alice"]);
    expect(f.costumeCalls).toEqual(["alice"]);
  });

  it("returns both-skipped without invoking the prepare helpers for an unknown participant", async () => {
    const manifest = parseManifestText(FIXTURE);
    const f = fakes();
    const events: PrimeProgress[] = [];
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "ghost",
      costumeSource,
      onProgress: (p) => events.push(p),
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(false);
    expect(result.costumePrimed).toBe(false);
    expect(result.skipped.audio).toBe(true);
    expect(result.skipped.costume).toBe(true);
    expect(f.audioCalls).toEqual([]);
    expect(f.costumeCalls).toEqual([]);
    expect(events).toHaveLength(1);
    expect(events[0].step).toBe("skipped");
    expect(events[0].message).toContain("ghost");
  });

  it("skips the costume step entirely when the participant has no costume_dir", async () => {
    const manifest = parseManifestText(FIXTURE);
    const f = fakes();
    // tina has no lines either, so both should be skipped.
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "tina",
      costumeSource,
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(false);
    expect(result.costumePrimed).toBe(false);
    expect(result.skipped.audio).toBe(true);
    expect(result.skipped.costume).toBe(true);
    expect(f.audioCalls).toEqual([]);
    expect(f.costumeCalls).toEqual([]);
  });

  it("surfaces a failed event and does NOT throw when prepareAudio crashes", async () => {
    const manifest = parseManifestText(FIXTURE);
    const f = fakes({ throwAudio: true });
    const events: PrimeProgress[] = [];
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      onProgress: (p) => events.push(p),
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(false);
    expect(result.skipped.audio).toBe(true);
    // The costume step still runs and succeeds — the auto-prime is
    // explicitly best-effort per kind.
    expect(result.costumePrimed).toBe(true);
    const failedEvent = events.find((e) => e.step === "failed");
    expect(failedEvent).toBeDefined();
    expect(failedEvent?.message).toContain("audio prep failed");
    expect(events.at(-1)?.step).toBe("done");
  });

  it("surfaces a failed event when prepareCostume crashes (audio still primes)", async () => {
    const manifest = parseManifestText(FIXTURE);
    const f = fakes({ throwCostume: true });
    const result = await ensureAssetsPrimed({
      manifest,
      manifestDir,
      runDir,
      participant: "alice",
      costumeSource,
      deps: { prepareAudio: f.prepareAudio, prepareCostume: f.prepareCostume },
    });
    expect(result.audioPrimed).toBe(true);
    expect(result.costumePrimed).toBe(false);
    expect(result.skipped.costume).toBe(true);
  });
});
