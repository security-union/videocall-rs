import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { beforeEach, describe, expect, it } from "vitest";

import { existsSync } from "node:fs";

import {
  deleteProfile,
  listProfiles,
  PROFILE_SCHEMA_VERSION,
  profilePath,
  ProfileExistsError,
  ProfileNotFoundError,
  ProfileValidationError,
  readProfile,
  renameProfile,
  saveProfile,
  type ProfileBotSpec,
} from "./profiles";

function sampleBot(participant = "alice"): ProfileBotSpec {
  return {
    meetingURL: "https://example.com/meeting/X",
    participant,
    displayName: participant,
    ttl: "5m",
    headless: false,
    network: "none",
    authBackend: "jwt",
    storageStateFile: undefined,
  };
}

describe("profilePath", () => {
  it("accepts alphanumeric + hyphen names", () => {
    const p = profilePath("/tmp/run", "demo-3-bots");
    expect(p.endsWith("/profiles/demo-3-bots.json")).toBe(true);
  });

  it("rejects names with dots", () => {
    expect(() => profilePath("/tmp/run", "../etc/passwd")).toThrow(ProfileValidationError);
  });

  it("rejects names starting with a hyphen", () => {
    expect(() => profilePath("/tmp/run", "-bad")).toThrow(ProfileValidationError);
  });

  it("rejects an overlong name", () => {
    const tooLong = "a".repeat(65);
    expect(() => profilePath("/tmp/run", tooLong)).toThrow(ProfileValidationError);
  });
});

describe("saveProfile / readProfile / listProfiles / deleteProfile", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "bots-profiles-test-"));
  });

  it("round-trips a saved profile", async () => {
    const saved = await saveProfile(dir, "demo-1", [sampleBot()]);
    expect(saved.name).toBe("demo-1");
    expect(saved.version).toBe(PROFILE_SCHEMA_VERSION);
    expect(saved.bots).toHaveLength(1);
    const reloaded = await readProfile(dir, "demo-1");
    expect(reloaded).toEqual(saved);
  });

  it("listProfiles returns saved profiles sorted by savedAt DESC", async () => {
    await saveProfile(dir, "older", [sampleBot("alice")]);
    // Wait a millisecond to guarantee a different timestamp.
    await new Promise((res) => setTimeout(res, 5));
    await saveProfile(dir, "newer", [sampleBot("bob")]);
    const list = await listProfiles(dir);
    expect(list.map((p) => p.name)).toEqual(["newer", "older"]);
  });

  it("listProfiles returns [] for a missing dir", async () => {
    const list = await listProfiles(join(dir, "no-such-dir"));
    expect(list).toEqual([]);
  });

  it("listProfiles falls back to a placeholder entry on malformed JSON", async () => {
    await saveProfile(dir, "ok", [sampleBot()]);
    // Hand-write a malformed file under the profiles dir.
    writeFileSync(profilePath(dir, "broken"), "{not json", "utf8");
    const list = await listProfiles(dir);
    expect(list.find((p) => p.name === "broken")?.botCount).toBe(0);
  });

  it("saveProfile throws ProfileExistsError on duplicate name", async () => {
    await saveProfile(dir, "demo", [sampleBot()]);
    await expect(saveProfile(dir, "demo", [sampleBot("bob")])).rejects.toThrow(ProfileExistsError);
  });

  it("readProfile throws ProfileNotFoundError when missing", async () => {
    await expect(readProfile(dir, "nope")).rejects.toThrow(ProfileNotFoundError);
  });

  it("deleteProfile removes the file", async () => {
    await saveProfile(dir, "gone", [sampleBot()]);
    await deleteProfile(dir, "gone");
    await expect(readProfile(dir, "gone")).rejects.toThrow(ProfileNotFoundError);
  });

  it("deleteProfile throws ProfileNotFoundError on missing", async () => {
    await expect(deleteProfile(dir, "nope")).rejects.toThrow(ProfileNotFoundError);
  });

  it("readProfile rejects a corrupted file via ProfileValidationError", async () => {
    // Make sure the profiles dir exists before hand-writing the
    // malformed file — `saveProfile` would have created it lazily,
    // but we're skipping that here to force the corruption.
    mkdirSync(join(dir, "profiles"), { recursive: true });
    const path = profilePath(dir, "corrupt");
    writeFileSync(path, '{"name":"corrupt","savedAt":42}', "utf8");
    await expect(readProfile(dir, "corrupt")).rejects.toThrow(ProfileValidationError);
  });

  it("persisted JSON keeps version + bots structure", async () => {
    await saveProfile(dir, "shape", [sampleBot("dave")]);
    const raw = JSON.parse(readFileSync(profilePath(dir, "shape"), "utf8"));
    expect(raw.version).toBe(PROFILE_SCHEMA_VERSION);
    expect(Array.isArray(raw.bots)).toBe(true);
    expect(raw.bots[0].participant).toBe("dave");
  });

  it("accepts authBackend: none in saved bots", async () => {
    await saveProfile(dir, "guest", [{ ...sampleBot("guest1"), authBackend: "none" }]);
    const p = await readProfile(dir, "guest");
    expect(p.bots[0].authBackend).toBe("none");
  });
});

describe("renameProfile", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "bots-profiles-rename-"));
  });

  it("renames a saved profile and updates the internal name field", async () => {
    const original = await saveProfile(dir, "demo-old", [sampleBot()]);
    const renamed = await renameProfile(dir, "demo-old", "demo-new");
    // Returned shape preserves savedAt + version, swaps name.
    expect(renamed.name).toBe("demo-new");
    expect(renamed.savedAt).toBe(original.savedAt);
    expect(renamed.version).toBe(original.version);
    // On disk: old file gone, new file present, both contain the
    // canonical name.
    expect(existsSync(profilePath(dir, "demo-old"))).toBe(false);
    expect(existsSync(profilePath(dir, "demo-new"))).toBe(true);
    const raw = JSON.parse(readFileSync(profilePath(dir, "demo-new"), "utf8"));
    expect(raw.name).toBe("demo-new");
  });

  it("listProfiles surfaces only the renamed file (old and new never coexist after success)", async () => {
    await saveProfile(dir, "before-rename", [sampleBot()]);
    await renameProfile(dir, "before-rename", "after-rename");
    const list = await listProfiles(dir);
    expect(list.map((p) => p.name)).toEqual(["after-rename"]);
  });

  it("readProfile resolves the renamed name and rejects the old one", async () => {
    await saveProfile(dir, "alpha", [sampleBot("alice")]);
    await renameProfile(dir, "alpha", "beta");
    const reloaded = await readProfile(dir, "beta");
    expect(reloaded.name).toBe("beta");
    expect(reloaded.bots[0].participant).toBe("alice");
    await expect(readProfile(dir, "alpha")).rejects.toThrow(ProfileNotFoundError);
  });

  it("rejects an invalid new name (regex)", async () => {
    await saveProfile(dir, "src", [sampleBot()]);
    await expect(renameProfile(dir, "src", "../escape")).rejects.toThrow(ProfileValidationError);
    // Source file still intact after the rejection.
    expect(existsSync(profilePath(dir, "src"))).toBe(true);
  });

  it("rejects an overlong new name", async () => {
    await saveProfile(dir, "src", [sampleBot()]);
    const tooLong = "a".repeat(65);
    await expect(renameProfile(dir, "src", tooLong)).rejects.toThrow(ProfileValidationError);
    expect(existsSync(profilePath(dir, "src"))).toBe(true);
  });

  it("rejects renaming a missing source with ProfileNotFoundError", async () => {
    await expect(renameProfile(dir, "ghost", "new-name")).rejects.toThrow(ProfileNotFoundError);
  });

  it("rejects when the new name collides with an existing profile (409)", async () => {
    await saveProfile(dir, "first", [sampleBot("alice")]);
    await saveProfile(dir, "second", [sampleBot("bob")]);
    await expect(renameProfile(dir, "first", "second")).rejects.toThrow(ProfileExistsError);
    // Both originals still exist on disk; no partial state.
    expect(existsSync(profilePath(dir, "first"))).toBe(true);
    expect(existsSync(profilePath(dir, "second"))).toBe(true);
  });

  it("rejects when the new name is the same as the current name", async () => {
    await saveProfile(dir, "same", [sampleBot()]);
    await expect(renameProfile(dir, "same", "same")).rejects.toThrow(ProfileValidationError);
  });

  it("preserves bot count + payload after rename", async () => {
    await saveProfile(dir, "multi", [sampleBot("alice"), sampleBot("bob"), sampleBot("carol")]);
    await renameProfile(dir, "multi", "multi-renamed");
    const reloaded = await readProfile(dir, "multi-renamed");
    expect(reloaded.bots).toHaveLength(3);
    expect(reloaded.bots.map((b) => b.participant)).toEqual(["alice", "bob", "carol"]);
  });
});
