import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, it, expect } from "vitest";

import { resolveAssetsForParticipant } from "./assets";
import { parseManifestText } from "./manifest";

const FIXTURE = `
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: tina
pause_ms: 0
lines:
- speaker: alice
  audio_file: lines/line_000.wav
- speaker: tina
  audio_file: lines/line_001.wav
`;

describe("resolveAssetsForParticipant", () => {
  let runDir: string;

  beforeEach(() => {
    runDir = mkdtempSync(join(tmpdir(), "bots-app-assets-test-"));
  });

  afterEach(() => {
    rmSync(runDir, { recursive: true, force: true });
  });

  it("returns null for both paths when prep-assets has not run", () => {
    const m = parseManifestText(FIXTURE);
    const a = resolveAssetsForParticipant({ manifest: m, runDir, participant: "alice" });
    expect(a.audioPath).toBeNull();
    expect(a.videoPath).toBeNull();
  });

  it("returns the audio path when only the WAV has been produced", () => {
    const m = parseManifestText(FIXTURE);
    const audioDir = join(runDir, "audio");
    mkdirSync(audioDir, { recursive: true });
    writeFileSync(join(audioDir, "alice.wav"), "RIFF");
    const a = resolveAssetsForParticipant({ manifest: m, runDir, participant: "alice" });
    expect(a.audioPath).toBe(join(audioDir, "alice.wav"));
    expect(a.videoPath).toBeNull();
  });

  it("returns both paths when WAV and y4m exist", () => {
    const m = parseManifestText(FIXTURE);
    const audioDir = join(runDir, "audio");
    const costumesDir = join(runDir, "costumes");
    mkdirSync(audioDir, { recursive: true });
    mkdirSync(costumesDir, { recursive: true });
    writeFileSync(join(audioDir, "alice.wav"), "RIFF");
    writeFileSync(join(costumesDir, "pirate.y4m"), "YUV4MPEG2");
    const a = resolveAssetsForParticipant({ manifest: m, runDir, participant: "alice" });
    expect(a.audioPath).toBe(join(audioDir, "alice.wav"));
    expect(a.videoPath).toBe(join(costumesDir, "pirate.y4m"));
  });

  it("returns null videoPath when participant has no costume_dir, even if some other y4m exists", () => {
    const m = parseManifestText(FIXTURE);
    const audioDir = join(runDir, "audio");
    const costumesDir = join(runDir, "costumes");
    mkdirSync(audioDir, { recursive: true });
    mkdirSync(costumesDir, { recursive: true });
    writeFileSync(join(audioDir, "tina.wav"), "RIFF");
    writeFileSync(join(costumesDir, "pirate.y4m"), "YUV4MPEG2");
    const a = resolveAssetsForParticipant({ manifest: m, runDir, participant: "tina" });
    expect(a.audioPath).toBe(join(audioDir, "tina.wav"));
    // tina has no costume_dir, so videoPath stays null regardless of what y4m
    // files happen to live in run/costumes/.
    expect(a.videoPath).toBeNull();
  });

  it("returns null for both paths when the participant is not in the manifest", () => {
    const m = parseManifestText(FIXTURE);
    const a = resolveAssetsForParticipant({ manifest: m, runDir, participant: "ghost" });
    expect(a.audioPath).toBeNull();
    expect(a.videoPath).toBeNull();
  });
});
