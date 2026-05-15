import { describe, expect, it } from "vitest";

import type { BotTask } from "../orchestrator";
import {
  generateBotId,
  newRegistryEntry,
  REGISTRY_RETENTION_MS,
  shortBotId,
  snapshotEntry,
  sweepStaleEntries,
  TERMINATED_REGISTRY_CAP,
} from "./registry";

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

function fakeTask(overrides: Partial<BotTask> = {}): BotTask {
  return {
    botId: generateBotId(),
    meetingURL: "https://example.com/meeting/X",
    participant: "alice",
    displayName: "Alice",
    headless: false,
    authBackend: "jwt",
    storageStateFile: null,
    ssoStateFile: null,
    manifest: null,
    runDir: null,
    ttl: 300_000,
    network: null,
    ...overrides,
  };
}

describe("generateBotId", () => {
  it("produces a v4 UUID", () => {
    expect(generateBotId()).toMatch(UUID_V4);
  });
  it("produces a fresh id each call", () => {
    const a = generateBotId();
    const b = generateBotId();
    expect(a).not.toBe(b);
  });
});

describe("shortBotId", () => {
  it("returns the first 8 chars", () => {
    expect(shortBotId("7f3b2d1e-1234-4567-89ab-cdef01234567")).toBe("7f3b2d1e");
  });
});

describe("newRegistryEntry", () => {
  it("starts in `launching` with handle=null", () => {
    const e = newRegistryEntry(fakeTask());
    expect(e.status).toBe("launching");
    expect(e.handle).toBeNull();
  });
  it("anchors ttlDeadline to now + ttl for finite ttls", () => {
    const before = Date.now();
    const e = newRegistryEntry(fakeTask({ ttl: 60_000 }));
    const after = Date.now();
    expect(e.ttlDeadline).not.toBeNull();
    expect(e.ttlDeadline!).toBeGreaterThanOrEqual(before + 60_000);
    expect(e.ttlDeadline!).toBeLessThanOrEqual(after + 60_000);
  });
  it("sets ttlDeadline to null for infinite ttl", () => {
    const e = newRegistryEntry(fakeTask({ ttl: "infinite" }));
    expect(e.ttlDeadline).toBeNull();
  });
});

describe("snapshotEntry", () => {
  it("computes ttlRemainingMs from deadline minus now", () => {
    const now = 1_000_000;
    const e = newRegistryEntry(fakeTask({ ttl: 60_000 }));
    e.ttlDeadline = now + 30_000;
    const snap = snapshotEntry(e, now);
    expect(snap.ttlRemainingMs).toBe(30_000);
  });
  it("clamps negative remaining to 0", () => {
    const now = 1_000_000;
    const e = newRegistryEntry(fakeTask({ ttl: 60_000 }));
    e.ttlDeadline = now - 5_000;
    const snap = snapshotEntry(e, now);
    expect(snap.ttlRemainingMs).toBe(0);
  });
  it("returns null ttlRemainingMs for infinite", () => {
    const e = newRegistryEntry(fakeTask({ ttl: "infinite" }));
    expect(snapshotEntry(e).ttlRemainingMs).toBeNull();
  });
  it("strips the live handle", () => {
    const e = newRegistryEntry(fakeTask());
    const snap = snapshotEntry(e);
    expect(snap).not.toHaveProperty("handle");
    expect(snap).not.toHaveProperty("task");
  });
  it("emits finishedAt=null while the bot is running", () => {
    const e = newRegistryEntry(fakeTask());
    e.status = "in-meeting";
    const snap = snapshotEntry(e);
    expect(snap.finishedAt).toBeNull();
  });
  it("emits the finishedAt timestamp once the entry transitions to done", () => {
    const e = newRegistryEntry(fakeTask());
    e.status = "done";
    e.finishedAt = 1_234_567;
    const snap = snapshotEntry(e);
    expect(snap.finishedAt).toBe(1_234_567);
  });
});

describe("sweepStaleEntries", () => {
  it("uses a 1-hour retention window", () => {
    // The Terminated Bots section relies on this being long enough to
    // be useful for post-mortem inspection. Lock it down to catch
    // accidental tightening back to the legacy 60s.
    expect(REGISTRY_RETENTION_MS).toBe(3_600_000);
  });
  it("drops done entries older than the retention window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "done";
    e.finishedAt = Date.now() - REGISTRY_RETENTION_MS - 1_000;
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(0);
  });
  it("retains a done entry for the full 1-hour window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "done";
    // 59 minutes after finish — still inside the window.
    e.finishedAt = Date.now() - 59 * 60_000;
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(1);
  });
  it("drops failed entries older than the retention window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "failed";
    e.finishedAt = Date.now() - REGISTRY_RETENTION_MS - 1_000;
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(0);
  });
  it("retains a failed entry for the full 1-hour window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "failed";
    e.finishedAt = Date.now() - 59 * 60_000;
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(1);
  });
  it("never drops in-flight entries", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "in-meeting";
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(1);
  });
  it("never drops in-flight entries even when the registry is over the cap", () => {
    const reg = new Map();
    // 100 terminated + 5 running. The running ones must survive.
    const baseFinish = Date.now() - 5_000;
    for (let i = 0; i < TERMINATED_REGISTRY_CAP + 50; i++) {
      const e = newRegistryEntry(fakeTask());
      e.status = "done";
      e.finishedAt = baseFinish - i;
      reg.set(e.botId, e);
    }
    const runningIds: string[] = [];
    for (let i = 0; i < 5; i++) {
      const e = newRegistryEntry(fakeTask());
      e.status = "in-meeting";
      reg.set(e.botId, e);
      runningIds.push(e.botId);
    }
    sweepStaleEntries(reg);
    for (const id of runningIds) {
      expect(reg.has(id)).toBe(true);
    }
  });
  it("evicts oldest-finished terminated entries when over the cap", () => {
    const reg = new Map();
    const now = Date.now();
    const youngestIds: string[] = [];
    const oldestIds: string[] = [];
    // 30 over-cap "old" entries — these should get dropped.
    for (let i = 0; i < 30; i++) {
      const e = newRegistryEntry(fakeTask());
      e.status = "done";
      // Old, but still inside the retention window.
      e.finishedAt = now - 10 * 60_000 - i;
      reg.set(e.botId, e);
      oldestIds.push(e.botId);
    }
    // 100 "young" entries — these should be preserved.
    for (let i = 0; i < TERMINATED_REGISTRY_CAP; i++) {
      const e = newRegistryEntry(fakeTask());
      e.status = "done";
      e.finishedAt = now - 1_000 - i;
      reg.set(e.botId, e);
      youngestIds.push(e.botId);
    }
    sweepStaleEntries(reg, now);
    expect(reg.size).toBe(TERMINATED_REGISTRY_CAP);
    for (const id of youngestIds) {
      expect(reg.has(id)).toBe(true);
    }
    for (const id of oldestIds) {
      expect(reg.has(id)).toBe(false);
    }
  });
  it("does not evict when under the cap", () => {
    const reg = new Map();
    const now = Date.now();
    for (let i = 0; i < TERMINATED_REGISTRY_CAP - 1; i++) {
      const e = newRegistryEntry(fakeTask());
      e.status = "done";
      e.finishedAt = now - 1_000;
      reg.set(e.botId, e);
    }
    sweepStaleEntries(reg, now);
    expect(reg.size).toBe(TERMINATED_REGISTRY_CAP - 1);
  });
});
