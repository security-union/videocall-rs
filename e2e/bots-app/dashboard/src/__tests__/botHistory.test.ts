import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  clearLaunchedBotHistory,
  loadLaunchedBotHistory,
  MAX_ENTRIES,
  recordLaunchedBot,
  removeLaunchedBot,
  runLocationLabelFor,
  STORAGE_KEY,
  type LaunchedBotHistoryEntry,
} from "../lib/botHistory";
import type { LaunchFormInitial } from "../components/LaunchForm";

/**
 * Stable fixture factory. Returns a fully-typed `LaunchFormInitial`
 * with `participant` injected — every other field stays at the
 * dashboard's empty defaults so the tests focus on history mechanics
 * rather than full-form combinatorics.
 */
function fixtureSpec(participant: string, overrides?: Partial<LaunchFormInitial>): LaunchFormInitial {
  return {
    meetingURL: "https://example.com/meeting/X",
    participant,
    displayName: "",
    ttl: "5m",
    network: "none",
    headless: false,
    authBackend: "jwt",
    storageStateFile: "",
    runLocation: "local",
    sshHostLabel: "",
    costume: "default",
    audio: "default",
    ...overrides,
  };
}

function fixtureEntry(participant: string, launchedAt: number): LaunchedBotHistoryEntry {
  const spec = fixtureSpec(participant);
  return {
    spec,
    launchedAt,
    meetingURL: spec.meetingURL,
    participant: spec.participant,
    runLocationLabel: runLocationLabelFor(spec),
  };
}

describe("botHistory storage helpers", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });
  afterEach(() => {
    window.localStorage.clear();
  });

  it("loadLaunchedBotHistory() returns [] when the key is unset", () => {
    expect(loadLaunchedBotHistory()).toEqual([]);
  });

  it("loadLaunchedBotHistory() returns [] when the JSON is malformed", () => {
    window.localStorage.setItem(STORAGE_KEY, "{not-json");
    expect(loadLaunchedBotHistory()).toEqual([]);
  });

  it("loadLaunchedBotHistory() returns [] when the parsed value isn't an array", () => {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify({ foo: "bar" }));
    expect(loadLaunchedBotHistory()).toEqual([]);
  });

  it("loadLaunchedBotHistory() drops entries that fail the structural check", () => {
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify([
        fixtureEntry("alice", 1000),
        { not: "an entry" },
        fixtureEntry("bob", 2000),
      ]),
    );
    const loaded = loadLaunchedBotHistory();
    expect(loaded).toHaveLength(2);
    expect(loaded.map((e) => e.participant)).toEqual(["bob", "alice"]);
  });

  it("recordLaunchedBot() persists the entry under STORAGE_KEY", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    const raw = window.localStorage.getItem(STORAGE_KEY);
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw!);
    expect(parsed).toHaveLength(1);
    expect(parsed[0].participant).toBe("alice");
  });

  it("recordLaunchedBot() sorts entries DESC by launchedAt on read", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    recordLaunchedBot(fixtureEntry("bob", 3000));
    recordLaunchedBot(fixtureEntry("carol", 2000));
    const loaded = loadLaunchedBotHistory();
    expect(loaded.map((e) => e.participant)).toEqual(["bob", "carol", "alice"]);
  });

  it("recordLaunchedBot() de-dupes by full-spec equality and bumps timestamp", () => {
    // Two entries with the same spec but different launchedAt: the
    // second write must NOT add a duplicate row — it should replace
    // the first one and keep only the newer timestamp.
    recordLaunchedBot(fixtureEntry("alice", 1000));
    recordLaunchedBot(fixtureEntry("alice", 5000));
    const loaded = loadLaunchedBotHistory();
    expect(loaded).toHaveLength(1);
    expect(loaded[0].launchedAt).toBe(5000);
  });

  it("recordLaunchedBot() treats different specs as distinct entries (no false-positive dedupe)", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    // Same participant but a different TTL — must NOT collapse.
    const otherSpec = fixtureSpec("alice", { ttl: "10m" });
    recordLaunchedBot({
      spec: otherSpec,
      launchedAt: 2000,
      meetingURL: otherSpec.meetingURL,
      participant: otherSpec.participant,
      runLocationLabel: runLocationLabelFor(otherSpec),
    });
    const loaded = loadLaunchedBotHistory();
    expect(loaded).toHaveLength(2);
  });

  it("recordLaunchedBot() caps the list at MAX_ENTRIES", () => {
    for (let i = 0; i < MAX_ENTRIES + 5; i++) {
      recordLaunchedBot(fixtureEntry(`bot-${i}`, i + 1));
    }
    const loaded = loadLaunchedBotHistory();
    expect(loaded).toHaveLength(MAX_ENTRIES);
    // Newest-first: the last bot we recorded is at the head.
    expect(loaded[0].participant).toBe(`bot-${MAX_ENTRIES + 4}`);
    // And the oldest survivors are bot-5 .. bot-(MAX_ENTRIES + 4); the
    // first five (bot-0 .. bot-4) were dropped.
    expect(loaded.at(-1)?.participant).toBe("bot-5");
  });

  it("removeLaunchedBot() drops the entry with matching launchedAt", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    recordLaunchedBot(fixtureEntry("bob", 2000));
    removeLaunchedBot(1000);
    const loaded = loadLaunchedBotHistory();
    expect(loaded.map((e) => e.participant)).toEqual(["bob"]);
  });

  it("removeLaunchedBot() is a no-op when no entry has that timestamp", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    removeLaunchedBot(9999);
    const loaded = loadLaunchedBotHistory();
    expect(loaded).toHaveLength(1);
  });

  it("clearLaunchedBotHistory() wipes the key", () => {
    recordLaunchedBot(fixtureEntry("alice", 1000));
    recordLaunchedBot(fixtureEntry("bob", 2000));
    clearLaunchedBotHistory();
    expect(loadLaunchedBotHistory()).toEqual([]);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("runLocationLabelFor() returns 'local' for local specs", () => {
    expect(runLocationLabelFor(fixtureSpec("alice"))).toBe("local");
  });

  it("runLocationLabelFor() returns 'ssh:<host>' when the host label is present", () => {
    expect(
      runLocationLabelFor(
        fixtureSpec("alice", { runLocation: "ssh", sshHostLabel: "mini-7" }),
      ),
    ).toBe("ssh:mini-7");
  });

  it("runLocationLabelFor() returns 'ssh' (no colon) when the SSH host label is blank", () => {
    expect(
      runLocationLabelFor(fixtureSpec("alice", { runLocation: "ssh", sshHostLabel: "  " })),
    ).toBe("ssh");
  });
});
