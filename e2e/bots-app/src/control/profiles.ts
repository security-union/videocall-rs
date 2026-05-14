import { existsSync } from "node:fs";
import { mkdir, readFile, readdir, stat, unlink, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";

import type { LaunchSpec } from "./server";

/**
 * Run profiles let an operator snapshot a set of bot launch
 * configurations and re-launch the whole group later with one click.
 * Phase 5.1: server-side persistence under
 * `<runDir>/profiles/<safeName>.json`.
 *
 * Wire shape (returned by GET /profiles/:name and POST /profiles):
 *
 *   {
 *     "name":     "demo-3-bots",
 *     "savedAt":  "2026-05-13T22:30:00.000Z",
 *     "bots":     [ <LaunchSpec>, <LaunchSpec>, … ],
 *     "version":  1
 *   }
 *
 * The `version` lets future schema bumps round-trip cleanly. Today's
 * loader accepts any version >= 1 and reads only the known fields.
 *
 * Filename safety: `<name>` is sanitized to alphanumeric + hyphen
 * before being used as a basename, and the resolved path is verified
 * to live inside the profiles directory so a profile named
 * "../../etc/passwd" can't escape.
 */

export const PROFILES_DIRNAME = "profiles";
export const PROFILE_SCHEMA_VERSION = 1;
const MAX_NAME_LEN = 64;
const NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9-]{0,63}$/;

export interface ProfileBotSpec {
  meetingURL: string;
  participant: string;
  displayName?: string;
  ttl: string;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
}

export interface RunProfile {
  name: string;
  savedAt: string;
  version: number;
  bots: ProfileBotSpec[];
}

export interface ProfileSummary {
  name: string;
  savedAt: string;
  botCount: number;
}

/**
 * Resolve `<runDir>/profiles/`. The directory is created lazily on
 * first save.
 */
export function profilesDir(runDir: string): string {
  return join(runDir, PROFILES_DIRNAME);
}

/**
 * Resolve the on-disk path for `name`. Throws on bad characters or a
 * resolved path that escapes the profiles dir.
 */
export function profilePath(runDir: string, name: string): string {
  if (!NAME_PATTERN.test(name) || name.length > MAX_NAME_LEN) {
    throw new ProfileValidationError(
      `profile name must match ${NAME_PATTERN.source} (got "${name}")`,
    );
  }
  const dir = resolve(profilesDir(runDir));
  const fsPath = resolve(dir, `${name}.json`);
  // `resolve(dir, …)` would happily walk out of `dir` if `name`
  // contained `..` or an absolute path — the pattern above already
  // forbids those characters, but we double-check via the
  // startsWith guard so a future loosening of `NAME_PATTERN` can't
  // silently introduce a path-escape bug.
  if (!fsPath.startsWith(dir + "/") && fsPath !== dir) {
    throw new ProfileValidationError(`profile name "${name}" resolves outside the profiles dir`);
  }
  return fsPath;
}

/**
 * Validation/sanitization error. The server maps this to HTTP 400.
 */
export class ProfileValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProfileValidationError";
  }
}

/**
 * Name-already-exists error. The server maps this to HTTP 409.
 */
export class ProfileExistsError extends Error {
  constructor(name: string) {
    super(`profile "${name}" already exists`);
    this.name = "ProfileExistsError";
  }
}

/**
 * Not-found error. The server maps this to HTTP 404.
 */
export class ProfileNotFoundError extends Error {
  constructor(name: string) {
    super(`profile "${name}" not found`);
    this.name = "ProfileNotFoundError";
  }
}

/**
 * Read every `<name>.json` under `<runDir>/profiles/` and return a
 * summary list sorted by `savedAt` DESC. Missing dir → empty list.
 */
export async function listProfiles(runDir: string): Promise<ProfileSummary[]> {
  const dir = profilesDir(runDir);
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw e;
  }
  const matches = entries.filter((f) => f.endsWith(".json"));
  const summaries = await Promise.all(
    matches.map(async (f) => {
      const fsPath = join(dir, f);
      try {
        const profile = await readProfileFile(fsPath);
        return {
          name: profile.name,
          savedAt: profile.savedAt,
          botCount: profile.bots.length,
        } satisfies ProfileSummary;
      } catch {
        // Malformed file — surface it as a placeholder rather than
        // crashing the whole list. The operator can fix or delete.
        const s = await stat(fsPath).catch(() => null);
        return {
          name: basename(f, ".json"),
          savedAt: s ? new Date(s.mtimeMs).toISOString() : new Date(0).toISOString(),
          botCount: 0,
        } satisfies ProfileSummary;
      }
    }),
  );
  summaries.sort((a, b) => (a.savedAt < b.savedAt ? 1 : -1));
  return summaries;
}

/**
 * Read a single profile by name. Throws `ProfileNotFoundError` when
 * the file doesn't exist, `ProfileValidationError` when JSON is
 * malformed.
 */
export async function readProfile(runDir: string, name: string): Promise<RunProfile> {
  const path = profilePath(runDir, name);
  if (!existsSync(path)) {
    throw new ProfileNotFoundError(name);
  }
  return readProfileFile(path);
}

async function readProfileFile(path: string): Promise<RunProfile> {
  const raw = await readFile(path, "utf8");
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new ProfileValidationError(`profile ${path} is not valid JSON: ${(e as Error).message}`);
  }
  if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new ProfileValidationError(`profile ${path} is not a JSON object`);
  }
  const obj = parsed as Record<string, unknown>;
  if (typeof obj.name !== "string") {
    throw new ProfileValidationError(`profile ${path}: "name" must be a string`);
  }
  if (typeof obj.savedAt !== "string") {
    throw new ProfileValidationError(`profile ${path}: "savedAt" must be a string`);
  }
  const version = typeof obj.version === "number" ? obj.version : 0;
  if (version < 1) {
    throw new ProfileValidationError(`profile ${path}: "version" must be >= 1`);
  }
  if (!Array.isArray(obj.bots)) {
    throw new ProfileValidationError(`profile ${path}: "bots" must be an array`);
  }
  const bots: ProfileBotSpec[] = obj.bots.map((entry, idx) =>
    validateBotSpec(entry, `${path} bots[${idx}]`),
  );
  return {
    name: obj.name,
    savedAt: obj.savedAt,
    version,
    bots,
  };
}

function validateBotSpec(entry: unknown, where: string): ProfileBotSpec {
  if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
    throw new ProfileValidationError(`${where} must be an object`);
  }
  const o = entry as Record<string, unknown>;
  const meetingURL = expectString(o.meetingURL, `${where}.meetingURL`);
  const participant = expectString(o.participant, `${where}.participant`);
  const ttl = expectString(o.ttl, `${where}.ttl`);
  const network = expectString(o.network, `${where}.network`);
  const auth = o.authBackend;
  if (auth !== "jwt" && auth !== "storage-state" && auth !== "none") {
    throw new ProfileValidationError(
      `${where}.authBackend must be "jwt", "storage-state", or "none"`,
    );
  }
  if (typeof o.headless !== "boolean") {
    throw new ProfileValidationError(`${where}.headless must be a boolean`);
  }
  const displayName =
    o.displayName === undefined ? undefined : expectString(o.displayName, `${where}.displayName`);
  const storageStateFile =
    o.storageStateFile === undefined
      ? undefined
      : expectString(o.storageStateFile, `${where}.storageStateFile`);
  return {
    meetingURL,
    participant,
    displayName,
    ttl,
    headless: o.headless,
    network,
    authBackend: auth,
    storageStateFile,
  };
}

function expectString(v: unknown, where: string): string {
  if (typeof v !== "string") {
    throw new ProfileValidationError(`${where} must be a string`);
  }
  return v;
}

/**
 * Persist a profile. Refuses to overwrite an existing one — callers
 * must delete first or pick a different name. Returns the canonical
 * `RunProfile` (with the resolved `savedAt`).
 */
export async function saveProfile(
  runDir: string,
  name: string,
  bots: ProfileBotSpec[],
): Promise<RunProfile> {
  const path = profilePath(runDir, name);
  if (existsSync(path)) {
    throw new ProfileExistsError(name);
  }
  await mkdir(profilesDir(runDir), { recursive: true });
  const profile: RunProfile = {
    name,
    savedAt: new Date().toISOString(),
    version: PROFILE_SCHEMA_VERSION,
    bots,
  };
  await writeFile(path, JSON.stringify(profile, null, 2), { encoding: "utf8", mode: 0o644 });
  return profile;
}

/**
 * Delete a profile. Idempotent on already-missing files.
 */
export async function deleteProfile(runDir: string, name: string): Promise<void> {
  const path = profilePath(runDir, name);
  try {
    await unlink(path);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") {
      throw new ProfileNotFoundError(name);
    }
    throw e;
  }
}

/**
 * Rename a profile by reading the existing file, updating its `name`
 * field to the new sanitized name, writing it to the new path, and
 * finally unlinking the old path.
 *
 * Atomicity strategy: write-then-unlink. We deliberately create the
 * new file BEFORE removing the old one so that a mid-operation failure
 * leaves a recoverable state on disk:
 *
 *   - If the write fails → the old file is still intact; the caller
 *     sees a 500 and the profile remains under its original name.
 *   - If the unlink fails → both files exist transiently; the new file
 *     IS the canonical one (its internal `name` field matches the new
 *     filename), and the caller sees a 500 noting the stale old file
 *     for manual cleanup.
 *
 * Rejects with `ProfileNotFoundError` when the source profile is
 * missing, and `ProfileExistsError` when a profile with `newName`
 * already exists (the operator must delete the conflicting file or
 * pick a different name; we do not silently overwrite).
 *
 * Returns the resulting `RunProfile` (with the renamed `name` field
 * and the original `savedAt` preserved — renaming is a metadata-only
 * operation, not a fresh save).
 */
export async function renameProfile(
  runDir: string,
  oldName: string,
  newName: string,
): Promise<RunProfile> {
  // Path resolution validates both names against `NAME_PATTERN` and
  // guards against path-escape, throwing `ProfileValidationError` on
  // bad input.
  const oldPath = profilePath(runDir, oldName);
  const newPath = profilePath(runDir, newName);
  if (oldName === newName) {
    throw new ProfileValidationError(`profile name "${newName}" is the same as the current name`);
  }
  if (!existsSync(oldPath)) {
    throw new ProfileNotFoundError(oldName);
  }
  if (existsSync(newPath)) {
    throw new ProfileExistsError(newName);
  }
  const existing = await readProfileFile(oldPath);
  const updated: RunProfile = {
    name: newName,
    savedAt: existing.savedAt,
    version: existing.version,
    bots: existing.bots,
  };
  // 1. Write the new file. If this throws the old file is untouched.
  await writeFile(newPath, JSON.stringify(updated, null, 2), { encoding: "utf8", mode: 0o644 });
  // 2. Remove the old file. If this throws the new file (canonical)
  //    is already on disk; we surface the stale-file problem so the
  //    operator can clean up manually.
  try {
    await unlink(oldPath);
  } catch (e) {
    throw new Error(
      `profile rename succeeded but failed to remove stale old file ${oldPath}: ${
        (e as Error).message
      } — the new profile "${newName}" is the canonical copy`,
      { cause: e },
    );
  }
  return updated;
}

/**
 * Translate a control-API `LaunchSpec` into the persisted
 * `ProfileBotSpec` shape. Drops anything we don't replay.
 */
export function launchSpecToProfileBot(spec: LaunchSpec): ProfileBotSpec {
  return {
    meetingURL: spec.meetingURL,
    participant: spec.participant,
    displayName: spec.displayName,
    ttl: typeof spec.ttl === "number" ? `${spec.ttl}ms` : spec.ttl,
    headless: spec.headless,
    network: spec.network,
    authBackend: spec.authBackend,
    storageStateFile: spec.storageStateFile,
  };
}
