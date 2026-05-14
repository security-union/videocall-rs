import { mkdtempSync, writeFileSync } from "node:fs";
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

describe("control server: SSH host registry endpoints", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    runDir = mkdtempSync(join(tmpdir(), "bots-server-hosts-"));
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("GET /hosts returns an empty list on a fresh runDir", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ hosts: [] });
  });

  it("POST /hosts creates a host and returns 201", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "mini-7",
        host: "mini-7.intra",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    expect(res.status).toBe(201);
    const body = res.body as { host: { label: string } };
    expect(body.host.label).toBe("mini-7");
  });

  it("POST /hosts rejects a duplicate label with 409", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "dup",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "dup",
        host: "h2",
        user: "bob",
        reposPath: "/home/bob/videocall",
      },
    });
    expect(res.status).toBe(409);
  });

  it("POST /hosts rejects invalid label with 400", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "-bad",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    expect(res.status).toBe(400);
  });

  it("PUT /hosts/:label patches a host", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "patch",
        host: "old",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts/patch", {
      method: "PUT",
      headers: { authorization: `Bearer ${token}` },
      body: { host: "new" },
    });
    expect(res.status).toBe(200);
    const body = res.body as { host: { host: string } };
    expect(body.host.host).toBe("new");
  });

  it("DELETE /hosts/:label removes the row", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "gone",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts/gone", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(204);
    const list = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(list.body).toEqual({ hosts: [] });
  });

  it("DELETE on a missing host returns 404", async () => {
    const res = await fetchJson(handle.port, "/hosts/ghost", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("GET /bots/:id/log returns empty list for a local bot", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/log`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ lines: [], totalLines: 0 });
  });

  it("GET /bots/:id/log returns 404 for an unknown bot id", async () => {
    const res = await fetchJson(handle.port, "/bots/nope/log", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("rejects unauthenticated requests against /hosts with 401", async () => {
    const res = await fetchJson(handle.port, "/hosts");
    expect(res.status).toBe(401);
  });

  it("returns 503 when runDir is not configured", async () => {
    const noRunDirHandle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
    });
    try {
      const res = await fetchJson(noRunDirHandle.port, "/hosts", {
        headers: { authorization: `Bearer ${token}` },
      });
      expect(res.status).toBe(503);
    } finally {
      await noRunDirHandle.close();
    }
  });

  it("listHosts surfaces malformed JSON on disk as 400", async () => {
    writeFileSync(join(runDir, "hosts.json"), "{ bad", "utf8");
    const res = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(400);
  });
});

describe("control server: launch with runLocation", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    runDir = mkdtempSync(join(tmpdir(), "bots-server-runloc-"));
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it('accepts runLocation = { kind: "local" } and forwards to surface.launchOne', async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "local" },
      },
    });
    expect(res.status).toBe(201);
    expect(surface.callLog.some((l) => l.startsWith("launch:"))).toBe(true);
  });

  it('accepts runLocation = { kind: "ssh", hostLabel }', async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "ssh", hostLabel: "mini-7" },
      },
    });
    expect(res.status).toBe(201);
    const launchEntry = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launchEntry).toBeDefined();
    expect(launchEntry!).toContain('"hostLabel":"mini-7"');
  });

  it("rejects runLocation.kind=ssh without hostLabel as 400", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "ssh" },
      },
    });
    expect(res.status).toBe(400);
  });

  it("rejects bare-string future-* runLocation values as 400", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: "future-vm",
      },
    });
    expect(res.status).toBe(400);
  });

  it("accepts bare-string 'local' for back-compat with pre-SSH clients", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: "local",
      },
    });
    expect(res.status).toBe(201);
  });
});
