import { describe, it, expect } from "vitest";

import {
  costumeNameForParticipant,
  firstNParticipantNames,
  lineAudioPath,
  linesForParticipant,
  parseManifestText,
} from "./manifest";

const FIXTURE = `
participants:
- name: alice
  voice: en-US-AvaNeural
  costume_dir: assets/costumes/pirate
- name: tina
  voice: en-GB-LibbyNeural
pause_ms: 800
lines:
- speaker: alice
  audio_file: lines/line_000.wav
  duration_ms: 5352
- speaker: tina
  audio_file: lines/line_001.wav
  duration_ms: 4200
- speaker: alice
  audio_file: lines/line_002.wav
`;

describe("parseManifestText", () => {
  it("parses participants and lines from the fixture", () => {
    const m = parseManifestText(FIXTURE);
    expect(m.participants).toEqual([
      { name: "alice", costumeDir: "assets/costumes/pirate" },
      { name: "tina", costumeDir: undefined },
    ]);
    expect(m.lines.length).toBe(3);
    expect(m.lines[0]).toEqual({
      speaker: "alice",
      audioFile: "lines/line_000.wav",
      durationMs: 5352,
    });
    expect(m.pauseMs).toBe(800);
  });

  it("rejects non-mapping input", () => {
    expect(() => parseManifestText("[]")).toThrow(/not a YAML mapping/);
  });

  it("rejects missing participants array", () => {
    expect(() => parseManifestText("lines: []")).toThrow(/participants must be an array/);
  });

  it("rejects empty participant name", () => {
    expect(() => parseManifestText("participants: [{ name: '' }]\nlines: []")).toThrow(
      /participants\[0\].name/,
    );
  });

  it("rejects non-string costume_dir", () => {
    expect(() =>
      parseManifestText("participants: [{ name: alice, costume_dir: 123 }]\nlines: []"),
    ).toThrow(/costume_dir must be a string/);
  });
});

describe("linesForParticipant", () => {
  it("filters in speaker order, preserves original line order", () => {
    const m = parseManifestText(FIXTURE);
    const aliceLines = linesForParticipant(m, "alice");
    expect(aliceLines.map((l) => l.audioFile)).toEqual([
      "lines/line_000.wav",
      "lines/line_002.wav",
    ]);
  });

  it("returns empty array for unknown participants", () => {
    const m = parseManifestText(FIXTURE);
    expect(linesForParticipant(m, "ghost")).toEqual([]);
  });

  it("returns empty array for voiceless participants in the manifest", () => {
    const m = parseManifestText(`
participants:
- name: silent
lines: []
`);
    expect(linesForParticipant(m, "silent")).toEqual([]);
  });
});

describe("costumeNameForParticipant", () => {
  it("returns the basename of costume_dir when present", () => {
    const m = parseManifestText(FIXTURE);
    expect(costumeNameForParticipant(m, "alice")).toBe("pirate");
  });

  it("returns undefined for participants without a costume_dir", () => {
    const m = parseManifestText(FIXTURE);
    expect(costumeNameForParticipant(m, "tina")).toBeUndefined();
  });

  it("returns undefined for unknown participants", () => {
    const m = parseManifestText(FIXTURE);
    expect(costumeNameForParticipant(m, "ghost")).toBeUndefined();
  });
});

describe("lineAudioPath", () => {
  it("joins the manifest directory with the audio_file path", () => {
    const m = parseManifestText(FIXTURE);
    const line = linesForParticipant(m, "alice")[0];
    expect(lineAudioPath("/tmp/conversation", line)).toBe("/tmp/conversation/lines/line_000.wav");
  });
});

describe("firstNParticipantNames", () => {
  it("returns the first N names in manifest order", () => {
    const m = parseManifestText(FIXTURE);
    expect(firstNParticipantNames(m, 1)).toEqual(["alice"]);
    expect(firstNParticipantNames(m, 2)).toEqual(["alice", "tina"]);
  });

  it("returns the empty array when N is 0", () => {
    const m = parseManifestText(FIXTURE);
    expect(firstNParticipantNames(m, 0)).toEqual([]);
  });

  it("returns all available names when N exceeds the participant count", () => {
    const m = parseManifestText(FIXTURE);
    expect(firstNParticipantNames(m, 99)).toEqual(["alice", "tina"]);
  });
});
