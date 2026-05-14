import { describe, expect, it } from "vitest";

import { countFailed } from "./orchestrator";
import { newRegistryEntry, type BotRegistryEntry } from "./control/registry";
import type { BotTask } from "./orchestrator";

function task(participant: string): BotTask {
  return {
    botId: `00000000-0000-0000-0000-${participant.padStart(12, "0")}`,
    meetingURL: "https://example.com/meeting/X",
    participant,
    displayName: participant,
    headless: true,
    authBackend: "jwt",
    ttl: 60_000,
  };
}

function entry(participant: string, mutate: (e: BotRegistryEntry) => void): BotRegistryEntry {
  const e = newRegistryEntry(task(participant));
  mutate(e);
  return e;
}

describe("orchestrator.countFailed (failure-tally classification)", () => {
  it("returns 0 for an empty registry", () => {
    expect(countFailed(new Map())).toBe(0);
  });

  it("does not count bots that completed normally (status=done, ttl-expired)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "done";
      e.finishReason = "ttl-expired";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(0);
  });

  it("does not count user-hangup graceful exits", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "done";
      e.finishReason = "user-hangup";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(0);
  });

  it("does not count waiting-room exits (parked by host's waiting room)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "done";
      e.finishReason = "waiting-room:waiting-room";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(0);
  });

  it("does not count waiting-for-host exits (host hasn't started yet)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "done";
      e.finishReason = "waiting-room:waiting-for-host";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(0);
  });

  it("counts meeting-rejected as a failure (host denied)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "failed";
      e.finishReason = "meeting-rejected:rejected";
      e.lastError = "host denied the join request";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(1);
  });

  it("counts meeting-error as a failure (server-reported error)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "failed";
      e.finishReason = "meeting-rejected:error";
      e.lastError = "host has left and no one can admit new participants";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(1);
  });

  it("counts launch-error as a failure (Chrome crash, timeout, ...)", () => {
    const registry = new Map<string, BotRegistryEntry>();
    const e = entry("alice", (e) => {
      e.status = "failed";
      e.finishReason = "launch-error";
      e.lastError = "page closed unexpectedly";
      e.finishedAt = Date.now();
    });
    registry.set(e.botId, e);
    expect(countFailed(registry)).toBe(1);
  });

  it("mixed registry: counts only the failed entries", () => {
    const registry = new Map<string, BotRegistryEntry>();
    for (const [p, mut] of [
      ["alice", (e: BotRegistryEntry) => ((e.status = "done"), (e.finishReason = "ttl-expired"))],
      [
        "bob",
        (e: BotRegistryEntry) => (
          (e.status = "done"),
          (e.finishReason = "waiting-room:waiting-room")
        ),
      ],
      [
        "carol",
        (e: BotRegistryEntry) => (
          (e.status = "failed"),
          (e.finishReason = "meeting-rejected:rejected")
        ),
      ],
      ["dave", (e: BotRegistryEntry) => ((e.status = "failed"), (e.finishReason = "launch-error"))],
      [
        "eve",
        (e: BotRegistryEntry) => (
          (e.status = "done"),
          (e.finishReason = "waiting-room:waiting-for-host")
        ),
      ],
    ] as const) {
      const e = entry(p, mut);
      registry.set(e.botId, e);
    }
    expect(countFailed(registry)).toBe(2);
  });
});
