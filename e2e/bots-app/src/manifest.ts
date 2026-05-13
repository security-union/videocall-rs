import { readFileSync } from "node:fs";
import { join, dirname, basename } from "node:path";

import { parse as parseYaml } from "yaml";

/**
 * Participant row from `bot/conversation/manifest.yaml`. The reused Rust
 * bot's manifest carries other fields (`voice`, etc.) that the browser bot
 * does not consume — only the ones the asset-prep step needs are typed.
 */
export interface ManifestParticipant {
  name: string;
  /**
   * `costume_dir` is optional in the manifest — some participants
   * (e.g. tina) intentionally lack a costume and would fall back to
   * EKG mode in the Rust bot. The browser bot treats them the same:
   * no y4m is produced for participants without a costume_dir.
   */
  costumeDir?: string;
}

export interface ManifestLine {
  speaker: string;
  audioFile: string;
  durationMs?: number;
}

export interface Manifest {
  participants: ManifestParticipant[];
  lines: ManifestLine[];
  pauseMs: number;
}

/**
 * Parse a manifest YAML buffer. Exposed as a pure function (no I/O) so the
 * vitest suite can exercise it with fixture strings.
 */
export function parseManifestText(text: string): Manifest {
  const raw = parseYaml(text);
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error("manifest is not a YAML mapping");
  }
  const obj = raw as Record<string, unknown>;

  if (!Array.isArray(obj.participants)) {
    throw new Error("manifest.participants must be an array");
  }
  if (!Array.isArray(obj.lines)) {
    throw new Error("manifest.lines must be an array");
  }

  const participants: ManifestParticipant[] = obj.participants.map((p: unknown, idx: number) => {
    const row = p as Record<string, unknown>;
    const name = row.name;
    if (typeof name !== "string" || name === "") {
      throw new Error(`manifest.participants[${idx}].name must be a non-empty string`);
    }
    const costumeDir = row.costume_dir;
    if (costumeDir !== undefined && typeof costumeDir !== "string") {
      throw new Error(`manifest.participants[${idx}].costume_dir must be a string when present`);
    }
    return {
      name,
      costumeDir: costumeDir as string | undefined,
    };
  });

  const lines: ManifestLine[] = obj.lines.map((line: unknown, idx: number) => {
    const row = line as Record<string, unknown>;
    const speaker = row.speaker;
    const audioFile = row.audio_file;
    if (typeof speaker !== "string" || speaker === "") {
      throw new Error(`manifest.lines[${idx}].speaker must be a non-empty string`);
    }
    if (typeof audioFile !== "string" || audioFile === "") {
      throw new Error(`manifest.lines[${idx}].audio_file must be a non-empty string`);
    }
    const durationMs = row.duration_ms;
    return {
      speaker,
      audioFile,
      durationMs: typeof durationMs === "number" ? durationMs : undefined,
    };
  });

  const pauseMs = typeof obj.pause_ms === "number" ? obj.pause_ms : 0;

  return { participants, lines, pauseMs };
}

/**
 * Load and parse a manifest from disk. Paths inside the manifest
 * (`audio_file`, `costume_dir`) remain relative — callers compose them
 * against `manifestDir` to get absolute paths.
 */
export function loadManifest(manifestPath: string): { manifest: Manifest; manifestDir: string } {
  const text = readFileSync(manifestPath, "utf8");
  const manifest = parseManifestText(text);
  return { manifest, manifestDir: dirname(manifestPath) };
}

/**
 * Resolve the audio_file paths for one participant, in order. Returns an
 * empty array when the participant exists in `participants` but has no
 * lines, or when the participant is not in the manifest at all (the latter
 * lets callers iterate over all participants without special-casing
 * voiceless ones).
 */
export function linesForParticipant(manifest: Manifest, participant: string): ManifestLine[] {
  return manifest.lines.filter((line) => line.speaker === participant);
}

/**
 * The first `n` participant names from the manifest, in manifest order.
 * Used by `bots-app run --users N` to fill a multi-bot meeting without
 * the operator having to enumerate each handle. Returns at most
 * `manifest.participants.length` names — callers should bound `n` first
 * if they want to error on over-subscription.
 */
export function firstNParticipantNames(manifest: Manifest, n: number): string[] {
  return manifest.participants.slice(0, n).map((p) => p.name);
}

/**
 * Costume name = basename of `costume_dir`. The Rust bot's manifest uses
 * `assets/costumes/<name>` paths relative to `bot/`; we only need the
 * trailing path component to look up `bot/assets/costumes/<name>/talking.mp4`
 * and to name our y4m output.
 */
export function costumeNameForParticipant(
  manifest: Manifest,
  participant: string,
): string | undefined {
  const row = manifest.participants.find((p) => p.name === participant);
  if (!row?.costumeDir) return undefined;
  return basename(row.costumeDir);
}

/**
 * Absolute path to a line's WAV given the manifest directory.
 */
export function lineAudioPath(manifestDir: string, line: ManifestLine): string {
  return join(manifestDir, line.audioFile);
}
