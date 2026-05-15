import { describe, expect, it } from "vitest";

import { buildLaunchedBotTask, countFailed } from "./orchestrator";
import {
  appendLocalLog,
  newRegistryEntry,
  readLocalLogWindow,
  type BotRegistryEntry,
  type BotStatus,
} from "./control/registry";
import type { LaunchSpec } from "./control/server";
import { parseManifestText } from "./manifest";
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

describe("orchestrator.buildLaunchedBotTask (dashboard /launch shape)", () => {
  const MANIFEST_FIXTURE = `
participants:
- name: alice
  costume_dir: assets/costumes/pirate
- name: tina
pause_ms: 0
lines:
- speaker: alice
  audio_file: lines/line_000.wav
`;

  function baseSpec(overrides: Partial<LaunchSpec> = {}): LaunchSpec {
    return {
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      ttl: 300_000,
      headless: false,
      network: "none",
      authBackend: "jwt",
      ...overrides,
    };
  }

  it("threads the orchestrator-loaded manifest + runDir into the BotTask", () => {
    const manifest = parseManifestText(MANIFEST_FIXTURE);
    const task = buildLaunchedBotTask(baseSpec(), {
      manifest,
      runDir: "/tmp/bots-app-run",
    });
    expect(task.manifest).toBe(manifest);
    expect(task.runDir).toBe("/tmp/bots-app-run");
    // No override picked → costumeOverride/audioOverride are null, so
    // bot.ts falls through to the manifest auto-match path.
    expect(task.costumeOverride).toBeNull();
    expect(task.audioOverride).toBeNull();
    expect(task.participant).toBe("alice");
    expect(task.displayName).toBe("Alice");
  });

  it("forwards explicit costume + audio overrides as basenames", () => {
    const manifest = parseManifestText(MANIFEST_FIXTURE);
    const task = buildLaunchedBotTask(baseSpec({ costume: "pirate.y4m", audio: "alice.wav" }), {
      manifest,
      runDir: "/tmp/bots-app-run",
    });
    expect(task.costumeOverride).toBe("pirate.y4m");
    expect(task.audioOverride).toBe("alice.wav");
    // Manifest still attached so a missing override file can fall
    // back to the auto-match path.
    expect(task.manifest).toBe(manifest);
  });

  it('collapses costume/audio="default" to null overrides (no operator pick)', () => {
    const task = buildLaunchedBotTask(baseSpec({ costume: "default", audio: "default" }), {
      manifest: null,
      runDir: "/tmp/bots-app-run",
    });
    expect(task.costumeOverride).toBeNull();
    expect(task.audioOverride).toBeNull();
  });

  it("falls back gracefully when the orchestrator has no manifest", () => {
    const task = buildLaunchedBotTask(baseSpec(), {
      manifest: null,
      runDir: "/tmp/bots-app-run",
    });
    expect(task.manifest).toBeNull();
    // runDir still propagates so an explicit override can still work
    // even without a manifest.
    expect(task.runDir).toBe("/tmp/bots-app-run");
  });

  it('collapses network="none" to null so ?netsim= is not appended', () => {
    const task = buildLaunchedBotTask(baseSpec({ network: "none" }), {
      manifest: null,
      runDir: null,
    });
    expect(task.network).toBeNull();
  });

  it("clears storageStateFile when authBackend is not storage-state", () => {
    const task = buildLaunchedBotTask(
      baseSpec({ authBackend: "jwt", storageStateFile: "/run/auth/alice.json" }),
      { manifest: null, runDir: null },
    );
    expect(task.storageStateFile).toBeNull();
  });

  it("threads the orchestrator-loaded manifestDir into the BotTask for auto-prime", () => {
    const manifest = parseManifestText(MANIFEST_FIXTURE);
    const task = buildLaunchedBotTask(baseSpec(), {
      manifest,
      manifestDir: "/repo/bot/conversation",
      runDir: "/tmp/bots-app-run",
    });
    expect(task.manifestDir).toBe("/repo/bot/conversation");
  });

  it("defaults manifestDir to null when omitted by the caller (back-compat)", () => {
    const task = buildLaunchedBotTask(baseSpec(), {
      manifest: null,
      runDir: null,
    });
    expect(task.manifestDir).toBeNull();
  });
});

describe("registry priming lifecycle", () => {
  it("BotStatus includes 'priming' as a valid value (auto-prime transition)", () => {
    // Type-level assertion: the union must permit 'priming'. Failing
    // this compile guards against accidental removal of the variant
    // during refactors.
    const s: BotStatus = "priming";
    expect(s).toBe("priming");
  });

  it("appendLocalLog populates a bot's rolling buffer (used by the auto-prime path)", () => {
    const e = newRegistryEntry({
      botId: "00000000-0000-0000-0000-000000000001",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      displayName: "Alice",
      headless: true,
      authBackend: "jwt",
      ttl: 60_000,
    });
    expect(e.recentLog).toEqual([]);
    expect(e.totalLines).toBe(0);
    appendLocalLog(e, "[alice] auto-prime: checking — looking at the on-disk state");
    appendLocalLog(e, "[alice] auto-prime: priming-audio — stitching 4 line(s)");
    appendLocalLog(e, "[alice] auto-prime: done — prime complete");
    expect(e.recentLog).toHaveLength(3);
    expect(e.totalLines).toBe(3);
    const window = readLocalLogWindow(e, 1);
    expect(window.lines).toHaveLength(2);
    expect(window.totalLines).toBe(3);
  });

  it("registry entries start in 'launching' state; priming is opt-in via the orchestrator", () => {
    const e = newRegistryEntry({
      botId: "00000000-0000-0000-0000-000000000002",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      displayName: "Alice",
      headless: true,
      authBackend: "jwt",
      ttl: 60_000,
    });
    // newRegistryEntry stays in 'launching'; the orchestrator's
    // `runSingleBotTask` flips it to 'priming' when the auto-prime
    // helper is going to run. This guarantees back-compat for the
    // SSH path (which sets status to 'in-meeting' directly and never
    // sees a 'priming' transition).
    expect(e.status).toBe("launching");
  });

  it("can transition through the documented priming → launching → joining → in-meeting → leaving → done chain", () => {
    const e: BotRegistryEntry = newRegistryEntry({
      botId: "00000000-0000-0000-0000-000000000003",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      displayName: "Alice",
      headless: true,
      authBackend: "jwt",
      ttl: 60_000,
    });
    const sequence: BotStatus[] = [
      "priming",
      "launching",
      "joining",
      "in-meeting",
      "leaving",
      "done",
    ];
    for (const status of sequence) {
      e.status = status;
      expect(e.status).toBe(status);
    }
  });
});
