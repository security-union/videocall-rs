import { describe, expect, it } from "vitest";

import type { BotTask } from "../orchestrator";
import {
  generateBotId,
  newRegistryEntry,
  REGISTRY_RETENTION_MS,
  shortBotId,
  snapshotEntry,
  sweepStaleEntries,
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
});

describe("sweepStaleEntries", () => {
  it("drops done entries older than the retention window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "done";
    e.finishedAt = Date.now() - REGISTRY_RETENTION_MS - 1_000;
    reg.set(e.botId, e);
    sweepStaleEntries(reg);
    expect(reg.size).toBe(0);
  });
  it("keeps done entries within the retention window", () => {
    const reg = new Map();
    const e = newRegistryEntry(fakeTask());
    e.status = "done";
    e.finishedAt = Date.now() - 1_000;
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
});
