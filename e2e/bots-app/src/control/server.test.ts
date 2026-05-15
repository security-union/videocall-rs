import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { BotTask } from "../orchestrator";
import { generateToken } from "./auth";
import {
  type ControlServerHandle,
  type OrchestratorControlSurface,
  startControlServer,
} from "./server";
import { type BotRegistryEntry, generateBotId, newRegistryEntry } from "./registry";

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

interface MockSurface extends OrchestratorControlSurface {
  registry: Map<string, BotRegistryEntry>;
  callLog: string[];
}

function mockSurface(initial: BotRegistryEntry[] = []): MockSurface {
  const registry = new Map<string, BotRegistryEntry>();
  for (const e of initial) registry.set(e.botId, e);
  const callLog: string[] = [];
  const surface: MockSurface = {
    registry,
    callLog,
    getRegistry: () => registry,
    triggerLeave: async (id) => void callLog.push(`leave:${id}`),
    forceKill: async (id) => void callLog.push(`kill:${id}`),
    applyTtl: (id, ttl) => void callLog.push(`ttl:${id}:${ttl}`),
    changeNetwork: async (id, n) => void callLog.push(`network:${id}:${n}`),
    setMicMuted: async (id, m) => void callLog.push(`mic:${id}:${m}`),
    setCameraOff: async (id, c) => void callLog.push(`cam:${id}:${c}`),
    setScreenShare: async (id, s) => void callLog.push(`share:${id}:${s}`),
    duplicateBot: async (id, ov) => {
      const newId = generateBotId();
      callLog.push(`dup:${id}->${newId}:${JSON.stringify(ov)}`);
      return newId;
    },
    launchOne: async (spec) => {
      const newId = generateBotId();
      callLog.push(`launch:${newId}:${JSON.stringify(spec)}`);
      return newId;
    },
  };
  return surface;
}

async function fetchJson(
  port: number,
  path: string,
  init: { method?: string; headers?: Record<string, string>; body?: unknown } = {},
): Promise<{ status: number; body: unknown }> {
  const headers: Record<string, string> = { accept: "application/json", ...init.headers };
  let body: string | undefined;
  if (init.body !== undefined) {
    body = JSON.stringify(init.body);
    headers["content-type"] = "application/json";
  }
  const res = await fetch(`http://127.0.0.1:${port}${path}`, {
    method: init.method ?? "GET",
    headers,
    body,
  });
  const text = await res.text();
  return {
    status: res.status,
    body: text.length === 0 ? null : JSON.parse(text),
  };
}

describe("control server", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    handle = await startControlServer({ port: 0, token, surface });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("binds to a free port when port=0", () => {
    expect(handle.port).toBeGreaterThan(0);
  });

  it("GET /healthz is unauthenticated and returns the live bot count", async () => {
    const res = await fetchJson(handle.port, "/healthz");
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ ok: true, bots: 0 });
  });

  it("rejects unauthenticated requests with 401", async () => {
    const res = await fetchJson(handle.port, "/bots");
    expect(res.status).toBe(401);
    expect(res.body).toEqual({ error: "unauthorized" });
  });

  it("rejects requests with the wrong bearer token", async () => {
    const res = await fetchJson(handle.port, "/bots", {
      headers: { authorization: "Bearer wrong-token" },
    });
    expect(res.status).toBe(401);
  });

  it("GET /bots returns the registered bots", async () => {
    const entry = newRegistryEntry(fakeTask({ participant: "bob" }));
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, "/bots", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as { bots: { botId: string; participant: string }[] };
    expect(body.bots).toHaveLength(1);
    expect(body.bots[0].participant).toBe("bob");
    expect(body.bots[0].botId).toBe(entry.botId);
  });

  it("GET /bots/:id returns 404 for unknown id", async () => {
    const res = await fetchJson(handle.port, "/bots/nope", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("POST /bots/:id/leave routes to surface.triggerLeave", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/leave`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(202);
    expect(surface.callLog).toContain(`leave:${entry.botId}`);
  });

  it("POST /bots/:id/ttl with `ttl` field sets absolute TTL", async () => {
    const entry = newRegistryEntry(fakeTask({ ttl: 60_000 }));
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/ttl`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { ttl: "10m" },
    });
    expect(res.status).toBe(200);
    expect(surface.callLog).toContain(`ttl:${entry.botId}:600000`);
  });

  it("POST /bots/:id/ttl with `extendBy` adds to remaining time", async () => {
    const entry = newRegistryEntry(fakeTask({ ttl: 60_000 }));
    // Pin the deadline so the test is deterministic.
    entry.ttlDeadline = Date.now() + 30_000;
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/ttl`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { extendBy: "1m" },
    });
    expect(res.status).toBe(200);
    // Expect the applied TTL to be approximately remaining (~30s) +
    // 60s = 90s. The control server does Math.max(0, deadline - now)
    // immediately before calling `applyTtl`, so allow a few ms slop.
    const match = surface.callLog.find((l) => l.startsWith(`ttl:${entry.botId}:`));
    expect(match).toBeDefined();
    const applied = Number.parseInt(match!.split(":")[2], 10);
    expect(applied).toBeGreaterThan(89_000);
    expect(applied).toBeLessThan(91_000);
  });

  it("POST /bots/:id/ttl rejects when neither field is supplied", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/ttl`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(res.status).toBe(400);
  });

  it("POST /bots/:id/network validates the netsim profile", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const bad = await fetchJson(handle.port, `/bots/${entry.botId}/network`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { network: "bogus" },
    });
    expect(bad.status).toBe(400);
    expect(surface.callLog).not.toContain(`network:${entry.botId}:bogus`);

    const ok = await fetchJson(handle.port, `/bots/${entry.botId}/network`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { network: "lossy_mobile" },
    });
    expect(ok.status).toBe(202);
    expect(surface.callLog).toContain(`network:${entry.botId}:lossy_mobile`);
  });

  it("POST /bots/:id/duplicate returns the new bot id", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/duplicate`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { participant: "frank", ttl: "5m" },
    });
    expect(res.status).toBe(201);
    const body = res.body as { botId: string };
    expect(body.botId).toMatch(/^[0-9a-f-]+$/);
    const dup = surface.callLog.find((l) => l.startsWith(`dup:`));
    expect(dup).toBeDefined();
    expect(dup!).toContain(`"participant":"frank"`);
    expect(dup!).toContain(`"ttl":300000`);
  });

  it("POST /bots/:id/duplicate rejects an unknown netsim profile", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/duplicate`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { network: "bogus" },
    });
    expect(res.status).toBe(400);
  });

  it("POST /bots/:id/mute requires the `mic` boolean", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const bad = await fetchJson(handle.port, `/bots/${entry.botId}/mute`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(bad.status).toBe(400);
    const ok = await fetchJson(handle.port, `/bots/${entry.botId}/mute`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { mic: true },
    });
    expect(ok.status).toBe(200);
    expect(surface.callLog).toContain(`mic:${entry.botId}:true`);
  });

  it("DELETE /bots/:id triggers forceKill", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(202);
    expect(surface.callLog).toContain(`kill:${entry.botId}`);
  });

  it("DELETE /bots/:id on a done bot is idempotent (200 + drops from registry, no forceKill)", async () => {
    const entry = newRegistryEntry(fakeTask());
    entry.status = "done";
    entry.finishedAt = Date.now();
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toMatchObject({ botId: entry.botId, action: "drop", removed: true });
    expect(surface.registry.has(entry.botId)).toBe(false);
    expect(surface.callLog.filter((l) => l.startsWith("kill:"))).toHaveLength(0);
  });

  it("DELETE /bots/:id on a failed bot is idempotent (200 + drops from registry)", async () => {
    const entry = newRegistryEntry(fakeTask());
    entry.status = "failed";
    entry.finishedAt = Date.now();
    entry.lastError = "join-rejected";
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(surface.registry.has(entry.botId)).toBe(false);
    expect(surface.callLog.filter((l) => l.startsWith("kill:"))).toHaveLength(0);
  });

  it("DELETE /bots/terminated removes all done/failed entries and leaves running ones alone", async () => {
    const running = newRegistryEntry(fakeTask({ participant: "running" }));
    running.status = "in-meeting";
    const done = newRegistryEntry(fakeTask({ participant: "done" }));
    done.status = "done";
    done.finishedAt = Date.now() - 1_000;
    const failed = newRegistryEntry(fakeTask({ participant: "failed" }));
    failed.status = "failed";
    failed.finishedAt = Date.now() - 2_000;
    surface.registry.set(running.botId, running);
    surface.registry.set(done.botId, done);
    surface.registry.set(failed.botId, failed);

    const res = await fetchJson(handle.port, "/bots/terminated", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ removedCount: 2 });
    expect(surface.registry.has(running.botId)).toBe(true);
    expect(surface.registry.has(done.botId)).toBe(false);
    expect(surface.registry.has(failed.botId)).toBe(false);
  });

  it("DELETE /bots/terminated on an empty registry is a 200 no-op", async () => {
    const res = await fetchJson(handle.port, "/bots/terminated", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ removedCount: 0 });
  });

  it("DELETE /bots/terminated requires the bearer token", async () => {
    const res = await fetchJson(handle.port, "/bots/terminated", { method: "DELETE" });
    expect(res.status).toBe(401);
  });

  it("GET /bots includes finishedAt: null for running bots and a number for terminated bots", async () => {
    const running = newRegistryEntry(fakeTask({ participant: "running" }));
    running.status = "in-meeting";
    const done = newRegistryEntry(fakeTask({ participant: "done" }));
    done.status = "done";
    // Anchor the finish to "now - 1s" so the route handler's sweep
    // doesn't evict the entry as stale before we inspect it.
    const finishedAt = Date.now() - 1_000;
    done.finishedAt = finishedAt;
    surface.registry.set(running.botId, running);
    surface.registry.set(done.botId, done);
    const res = await fetchJson(handle.port, "/bots", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as { bots: Array<{ botId: string; finishedAt: number | null }> };
    const runningSnap = body.bots.find((b) => b.botId === running.botId)!;
    const doneSnap = body.bots.find((b) => b.botId === done.botId)!;
    expect(runningSnap.finishedAt).toBeNull();
    expect(doneSnap.finishedAt).toBe(finishedAt);
  });

  it("POST /bots/:id/share requires the `share` boolean", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const bad = await fetchJson(handle.port, `/bots/${entry.botId}/share`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(bad.status).toBe(400);
    const ok = await fetchJson(handle.port, `/bots/${entry.botId}/share`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { share: true },
    });
    expect(ok.status).toBe(200);
    expect(surface.callLog).toContain(`share:${entry.botId}:true`);
  });

  it("POST /launch validates required fields", async () => {
    const bad = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { meetingURL: "https://example.com/meeting/X" },
    });
    expect(bad.status).toBe(400);
    expect(surface.callLog.filter((l) => l.startsWith("launch:"))).toHaveLength(0);
  });

  it("POST /launch rejects an unknown netsim profile", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "bogus",
        authBackend: "jwt",
      },
    });
    expect(res.status).toBe(400);
  });

  it("POST /launch rejects non-local runLocation", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        runLocation: "future-vm",
      },
    });
    expect(res.status).toBe(400);
  });

  it("POST /launch routes to surface.launchOne and returns 201", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
      },
    });
    expect(res.status).toBe(201);
    const body = res.body as { botId: string };
    expect(body.botId).toMatch(/^[0-9a-f-]+$/);
    const launch = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launch).toBeDefined();
    expect(launch!).toContain(`"participant":"alice"`);
    expect(launch!).toContain(`"network":"none"`);
  });

  it('POST /launch accepts authBackend: "none" (guest)', async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "none",
      },
    });
    expect(res.status).toBe(201);
    const launch = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launch).toBeDefined();
    expect(launch!).toContain(`"authBackend":"none"`);
  });

  it("POST /launch forwards costume + audio overrides to surface.launchOne", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        costume: "pirate.y4m",
        audio: "alice.wav",
      },
    });
    expect(res.status).toBe(201);
    const launch = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launch).toBeDefined();
    expect(launch!).toContain(`"costume":"pirate.y4m"`);
    expect(launch!).toContain(`"audio":"alice.wav"`);
  });

  it("POST /launch rejects costume containing directory traversal", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        costume: "../etc/passwd",
      },
    });
    expect(res.status).toBe(400);
    expect(surface.callLog.filter((l) => l.startsWith("launch:"))).toHaveLength(0);
  });

  it("POST /launch rejects costume with path separators", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        costume: "subdir/pirate.y4m",
      },
    });
    expect(res.status).toBe(400);
  });

  it("POST /launch rejects audio with the wrong extension", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        audio: "alice.mp3",
      },
    });
    expect(res.status).toBe(400);
  });

  it('POST /launch accepts costume: "default" as the sentinel for no override', async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        costume: "default",
        audio: "default",
      },
    });
    expect(res.status).toBe(201);
  });

  it("POST /launch with no costume/audio omits them from the launch spec (orchestrator uses manifest auto-match)", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
      },
    });
    expect(res.status).toBe(201);
    const launch = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launch).toBeDefined();
    // costume + audio MUST be absent from the spec so the orchestrator
    // can distinguish "operator made no pick" (apply auto-match) from
    // "operator picked default" (still no override, but the form was
    // touched).
    expect(launch!).not.toMatch(/"costume":/);
    expect(launch!).not.toMatch(/"audio":/);
  });

  it("POST /launch rejects an unknown authBackend value", async () => {
    const res = await fetchJson(handle.port, `/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "cookies",
      },
    });
    expect(res.status).toBe(400);
  });

  it("returns 404 for unknown routes", async () => {
    const res = await fetchJson(handle.port, "/nope", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });
});

describe("control server: run profiles", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    runDir = mkdtempSync(join(tmpdir(), "bots-profiles-"));
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("GET /profiles returns an empty list before any profile is saved", async () => {
    const res = await fetchJson(handle.port, "/profiles", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ profiles: [] });
  });

  it("POST /profiles with source=current snapshots the live registry", async () => {
    const entry = newRegistryEntry(fakeTask({ participant: "alice" }));
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "demo-1", source: "current" },
    });
    expect(res.status).toBe(201);
    const profile = res.body as { name: string; bots: { participant: string }[] };
    expect(profile.name).toBe("demo-1");
    expect(profile.bots).toHaveLength(1);
    expect(profile.bots[0].participant).toBe("alice");
  });

  it("POST /profiles with source=current captures each bot's runLocation from the registry", async () => {
    // Snapshot must serialize the per-bot host so a later launch can
    // dispatch the bot back to the same place. Mix local + ssh
    // entries so both branches are covered.
    const localEntry = newRegistryEntry(fakeTask({ participant: "alice" }), { kind: "local" });
    const sshEntry = newRegistryEntry(fakeTask({ participant: "bob" }), {
      kind: "ssh",
      hostLabel: "lab-mac-1",
    });
    surface.registry.set(localEntry.botId, localEntry);
    surface.registry.set(sshEntry.botId, sshEntry);
    const res = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "mixed-snap", source: "current" },
    });
    expect(res.status).toBe(201);
    const profile = res.body as {
      bots: { participant: string; runLocation?: { kind: string; hostLabel?: string } }[];
    };
    const alice = profile.bots.find((b) => b.participant === "alice");
    const bob = profile.bots.find((b) => b.participant === "bob");
    expect(alice?.runLocation).toEqual({ kind: "local" });
    expect(bob?.runLocation).toEqual({ kind: "ssh", hostLabel: "lab-mac-1" });
  });

  it("POST /profiles rejects source=current when no bots are running", async () => {
    const res = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "demo-empty", source: "current" },
    });
    expect(res.status).toBe(400);
  });

  it("POST /profiles rejects a name with bad characters", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "../escape", source: "current" },
    });
    expect(res.status).toBe(400);
  });

  it("POST /profiles refuses to overwrite an existing profile (409)", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const ok = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "dup", source: "current" },
    });
    expect(ok.status).toBe(201);
    const dup = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "dup", source: "current" },
    });
    expect(dup.status).toBe(409);
  });

  it("GET /profiles/:name returns the saved profile, DELETE removes it", async () => {
    const entry = newRegistryEntry(fakeTask({ participant: "carol" }));
    surface.registry.set(entry.botId, entry);
    await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "carol-only", source: "current" },
    });
    const got = await fetchJson(handle.port, `/profiles/carol-only`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(got.status).toBe(200);
    const list = await fetchJson(handle.port, `/profiles`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect((list.body as { profiles: unknown[] }).profiles).toHaveLength(1);
    const del = await fetchJson(handle.port, `/profiles/carol-only`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(del.status).toBe(200);
    const after = await fetchJson(handle.port, `/profiles`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect((after.body as { profiles: unknown[] }).profiles).toHaveLength(0);
  });

  it("POST /profiles/:name/launch forwards each bot's runLocation to launchOne", async () => {
    // Save a profile with mixed local + ssh bots, then verify every
    // launchOne call receives the captured runLocation verbatim. This
    // is the fix for the bug where a profile that captured an SSH
    // bot would re-launch it locally.
    const saveRes = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        name: "mixed-runloc",
        source: {
          bots: [
            {
              meetingURL: "https://example.com/meeting/X",
              participant: "alice",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
              runLocation: { kind: "local" },
            },
            {
              meetingURL: "https://example.com/meeting/X",
              participant: "bob",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
              runLocation: { kind: "ssh", hostLabel: "lab-mac-1" },
            },
          ],
        },
      },
    });
    expect(saveRes.status).toBe(201);
    const launchRes = await fetchJson(handle.port, `/profiles/mixed-runloc/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(launchRes.status).toBe(202);
    const launches = surface.callLog.filter((l) => l.startsWith("launch:"));
    expect(launches).toHaveLength(2);
    expect(launches[0]).toContain('"runLocation":{"kind":"local"}');
    expect(launches[1]).toContain('"runLocation":{"kind":"ssh","hostLabel":"lab-mac-1"}');
  });

  it("POST /profiles/:name/launch defaults missing runLocation to local (legacy forward-compat)", async () => {
    // Forward-compat: a saved profile that predates the runLocation
    // field still launches successfully; the launch route fills in a
    // local default so no orchestrator path observes `undefined`.
    const saveRes = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        name: "legacy-runloc",
        source: {
          bots: [
            {
              meetingURL: "https://example.com/meeting/X",
              participant: "alice",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
              // No runLocation -- mirrors a legacy on-disk profile.
            },
          ],
        },
      },
    });
    expect(saveRes.status).toBe(201);
    const launchRes = await fetchJson(handle.port, `/profiles/legacy-runloc/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(launchRes.status).toBe(202);
    const launches = surface.callLog.filter((l) => l.startsWith("launch:"));
    expect(launches).toHaveLength(1);
    expect(launches[0]).toContain('"runLocation":{"kind":"local"}');
  });

  it("POST /profiles/:name/launch fans out launchOne for every bot in the profile", async () => {
    // Save a profile via explicit bots[] so the test doesn't depend on
    // the registry path.
    const saveRes = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        name: "two-bots",
        source: {
          bots: [
            {
              meetingURL: "https://example.com/meeting/X",
              participant: "alice",
              ttl: "5m",
              headless: false,
              network: "none",
              authBackend: "jwt",
            },
            {
              meetingURL: "https://example.com/meeting/X",
              participant: "bob",
              ttl: "10m",
              headless: false,
              network: "none",
              authBackend: "none",
            },
          ],
        },
      },
    });
    expect(saveRes.status).toBe(201);
    const launchRes = await fetchJson(handle.port, `/profiles/two-bots/launch`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(launchRes.status).toBe(202);
    const body = launchRes.body as { name: string; botIds: string[] };
    expect(body.name).toBe("two-bots");
    expect(body.botIds).toHaveLength(2);
    const launches = surface.callLog.filter((l) => l.startsWith("launch:"));
    expect(launches).toHaveLength(2);
    expect(launches[0]).toContain(`"participant":"alice"`);
    expect(launches[1]).toContain(`"participant":"bob"`);
    expect(launches[1]).toContain(`"authBackend":"none"`);
  });

  it("GET /profiles/:name returns 404 for unknown profile", async () => {
    const res = await fetchJson(handle.port, `/profiles/nope`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("returns 503 on /profiles when the server has no runDir", async () => {
    const otherHandle = await startControlServer({ port: 0, token, surface });
    try {
      const res = await fetchJson(otherHandle.port, "/profiles", {
        headers: { authorization: `Bearer ${token}` },
      });
      expect(res.status).toBe(503);
    } finally {
      await otherHandle.close();
    }
  });

  it("POST /profiles/:name/rename moves a saved profile to a new name and updates its name field", async () => {
    const entry = newRegistryEntry(fakeTask({ participant: "alice" }));
    surface.registry.set(entry.botId, entry);
    const saveRes = await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "demo-old", source: "current" },
    });
    expect(saveRes.status).toBe(201);
    const renameRes = await fetchJson(handle.port, `/profiles/demo-old/rename`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { newName: "demo-new" },
    });
    expect(renameRes.status).toBe(200);
    const profile = renameRes.body as { name: string; bots: { participant: string }[] };
    expect(profile.name).toBe("demo-new");
    expect(profile.bots[0].participant).toBe("alice");

    // Old name returns 404, new name fetches the same payload.
    const oldGet = await fetchJson(handle.port, `/profiles/demo-old`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(oldGet.status).toBe(404);
    const newGet = await fetchJson(handle.port, `/profiles/demo-new`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(newGet.status).toBe(200);
    expect((newGet.body as { name: string }).name).toBe("demo-new");
  });

  it("POST /profiles/:name/rename rejects a missing newName with 400", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "src", source: "current" },
    });
    const res = await fetchJson(handle.port, `/profiles/src/rename`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(res.status).toBe(400);
  });

  it("POST /profiles/:name/rename rejects a newName with bad characters with 400", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "src", source: "current" },
    });
    const res = await fetchJson(handle.port, `/profiles/src/rename`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { newName: "../escape" },
    });
    expect(res.status).toBe(400);
  });

  it("POST /profiles/:name/rename returns 404 when the source profile is missing", async () => {
    const res = await fetchJson(handle.port, `/profiles/ghost/rename`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { newName: "ghost-renamed" },
    });
    expect(res.status).toBe(404);
  });

  it("POST /profiles/:name/rename returns 409 when newName collides with an existing profile", async () => {
    const entry = newRegistryEntry(fakeTask({ participant: "alice" }));
    surface.registry.set(entry.botId, entry);
    await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "first", source: "current" },
    });
    await fetchJson(handle.port, `/profiles`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { name: "second", source: "current" },
    });
    const res = await fetchJson(handle.port, `/profiles/first/rename`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { newName: "second" },
    });
    expect(res.status).toBe(409);
    // Both profiles still listed; no partial-state collision.
    const list = await fetchJson(handle.port, `/profiles`, {
      headers: { authorization: `Bearer ${token}` },
    });
    const names = (list.body as { profiles: { name: string }[] }).profiles.map((p) => p.name);
    expect(names.sort()).toEqual(["first", "second"]);
  });
});

describe("GET /assets/manifest", () => {
  // The endpoint reads the manifest YAML at request time and stat()s
  // every candidate y4m / wav under <runDir>/{costumes,audio}. The
  // tests below build a temp manifest + temp runDir and assert the
  // wire response one cell at a time.
  let token: string;
  let runDir: string;
  let manifestPath: string;
  let handle: ControlServerHandle;

  beforeEach(() => {
    token = generateToken();
    runDir = mkdtempSync(join(tmpdir(), "bots-ctl-manifest-"));
    mkdirSync(join(runDir, "costumes"));
    mkdirSync(join(runDir, "audio"));
    manifestPath = join(runDir, "manifest.yaml");
  });

  afterEach(async () => {
    if (handle) await handle.close();
  });

  it("returns participants with matching costume + audio files", async () => {
    // alice: has both a costume and an audio file on disk.
    // bob:   manifest entry has no costume_dir AND no audio file on disk.
    writeFileSync(
      manifestPath,
      `participants:
  - name: alice
    costume_dir: assets/costumes/pirate
  - name: bob
lines:
  - speaker: alice
    audio_file: alice/1.wav
  - speaker: bob
    audio_file: bob/1.wav
pause_ms: 0
`,
    );
    writeFileSync(join(runDir, "costumes", "pirate.y4m"), "");
    writeFileSync(join(runDir, "audio", "alice.wav"), "");
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      manifestPath,
    });
    const res = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({
      participants: [
        { name: "alice", costumeFile: "pirate.y4m", audioFile: "alice.wav" },
        { name: "bob", costumeFile: null, audioFile: null },
      ],
    });
  });

  it("returns null costumeFile when manifest assigns a costume but the y4m is missing on disk", async () => {
    writeFileSync(
      manifestPath,
      `participants:
  - name: carol
    costume_dir: assets/costumes/wizard
lines:
  - speaker: carol
    audio_file: carol/1.wav
pause_ms: 0
`,
    );
    // No wizard.y4m on disk — only an audio file.
    writeFileSync(join(runDir, "audio", "carol.wav"), "");
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      manifestPath,
    });
    const res = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({
      participants: [{ name: "carol", costumeFile: null, audioFile: "carol.wav" }],
    });
  });

  it("returns an empty participants list when the manifest path is unset", async () => {
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
    });
    const res = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ participants: [] });
  });

  it("returns an empty participants list when the manifest file does not exist on disk", async () => {
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      manifestPath: join(runDir, "does-not-exist.yaml"),
    });
    const res = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ participants: [] });
  });

  it("requires auth", async () => {
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      manifestPath,
    });
    const res = await fetchJson(handle.port, "/assets/manifest");
    expect(res.status).toBe(401);
  });

  it("caches the response so a stat'd file flipping between requests is not seen until TTL expires", async () => {
    writeFileSync(
      manifestPath,
      `participants:
  - name: dave
    costume_dir: assets/costumes/cowboy
lines:
  - speaker: dave
    audio_file: dave/1.wav
pause_ms: 0
`,
    );
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      manifestPath,
    });
    // First call: no costume, no audio.
    const first = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(first.body).toEqual({
      participants: [{ name: "dave", costumeFile: null, audioFile: null }],
    });
    // Drop the files in between — within the 30s cache window the
    // endpoint should still return the prior snapshot.
    writeFileSync(join(runDir, "costumes", "cowboy.y4m"), "");
    writeFileSync(join(runDir, "audio", "dave.wav"), "");
    const second = await fetchJson(handle.port, "/assets/manifest", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(second.body).toEqual(first.body);
  });
});
