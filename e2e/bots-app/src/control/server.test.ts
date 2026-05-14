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
    duplicateBot: async (id, ov) => {
      const newId = generateBotId();
      callLog.push(`dup:${id}->${newId}:${JSON.stringify(ov)}`);
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

  it("returns 404 for unknown routes", async () => {
    const res = await fetchJson(handle.port, "/nope", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });
});
