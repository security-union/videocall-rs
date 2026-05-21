import { mkdirSync, mkdtempSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { defaultSsoStatePath } from "../auth/storage-state";
import { generateToken } from "./auth";
import {
  type ControlServerHandle,
  type OrchestratorControlSurface,
  startControlServer,
} from "./server";
import type { SsoCaptureSession } from "../auth/sso-capture";
import { type BotRegistryEntry, generateBotId } from "./registry";

function mockSurface(): OrchestratorControlSurface {
  const registry = new Map<string, BotRegistryEntry>();
  return {
    getRegistry: () => registry,
    triggerLeave: async () => {},
    forceKill: async () => {},
    applyTtl: () => {},
    changeNetwork: async () => {},
    setMicMuted: async () => {},
    setCameraOff: async () => {},
    setScreenShare: async () => {},
    duplicateBot: async () => generateBotId(),
    launchOne: async () => generateBotId(),
  };
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

/**
 * Build a Playwright-shaped stub for {@link SsoCaptureSession} so the
 * tests can drive the recapture lifecycle without launching a real
 * Chromium. Records whether `saveAndClose` / `close` was called so the
 * idle-timeout and cancel paths can be asserted.
 */
function stubSession(): SsoCaptureSession & {
  saved: boolean;
  closed: boolean;
  savedPath: string | null;
} {
  const obj: SsoCaptureSession & {
    saved: boolean;
    closed: boolean;
    savedPath: string | null;
  } = {
    // The tests never touch the real browser/context handles — we
    // expose them as opaque objects so the type-check is satisfied.
    browser: {} as never,
    context: {} as never,
    saveAndClose: async (path: string): Promise<void> => {
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, JSON.stringify({ cookies: [], origins: [] }));
      obj.saved = true;
      obj.closed = true;
      obj.savedPath = path;
    },
    close: async (): Promise<void> => {
      obj.closed = true;
    },
    saved: false,
    closed: false,
    savedPath: null,
  };
  return obj;
}

describe("control server: VPN status", () => {
  let handle: ControlServerHandle;
  let token: string;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    runDir = mkdtempSync(join(tmpdir(), "bots-sso-"));
  });

  afterEach(async () => {
    await handle.close();
    rmSync(runDir, { recursive: true, force: true });
  });

  it('classifies a 200 OK as "up"', async () => {
    const stub = vi.fn().mockResolvedValue(new Response(null, { status: 200 }));
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: stub as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as { status: string; checkedAt: number; responseTimeMs: number };
    expect(body.status).toBe("up");
    expect(body.checkedAt).toBeGreaterThan(0);
    expect(stub).toHaveBeenCalledOnce();
  });

  it('classifies a 401 as "up" (VPN reachable, just no session)', async () => {
    const stub = vi.fn().mockResolvedValue(new Response(null, { status: 401 }));
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: stub as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect((res.body as { status: string }).status).toBe("up");
  });

  it('classifies an HTTP 503 as "down"', async () => {
    const stub = vi.fn().mockResolvedValue(new Response(null, { status: 503 }));
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: stub as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status", {
      headers: { authorization: `Bearer ${token}` },
    });
    const body = res.body as { status: string; error: string };
    expect(body.status).toBe("down");
    expect(body.error).toBe("HTTP 503");
  });

  it('classifies an ENOTFOUND as "down" with a DNS error message', async () => {
    const stub = vi.fn().mockRejectedValue(
      Object.assign(new Error("getaddrinfo ENOTFOUND nowhere.invalid"), {
        name: "FetchError",
      }),
    );
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: stub as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status", {
      headers: { authorization: `Bearer ${token}` },
    });
    const body = res.body as { status: string; error: string };
    expect(body.status).toBe("down");
    expect(body.error).toMatch(/DNS/);
  });

  it('classifies an AbortError as "down" with "timeout"', async () => {
    const stub = vi
      .fn()
      .mockImplementation(() =>
        Promise.reject(Object.assign(new Error("aborted"), { name: "AbortError" })),
      );
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: stub as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status", {
      headers: { authorization: `Bearer ${token}` },
    });
    const body = res.body as { status: string; error: string };
    expect(body.status).toBe("down");
    expect(body.error).toBe("timeout");
  });

  it("rejects unauthenticated requests with 401", async () => {
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      vpnFetch: vi.fn() as unknown as typeof fetch,
    });
    const res = await fetchJson(handle.port, "/sso/vpn-status");
    expect(res.status).toBe(401);
  });
});

describe("control server: SSO status", () => {
  let handle: ControlServerHandle;
  let token: string;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    runDir = mkdtempSync(join(tmpdir(), "bots-sso-"));
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
    });
  });

  afterEach(async () => {
    await handle.close();
    rmSync(runDir, { recursive: true, force: true });
  });

  it("reports exists=false when the file is missing", async () => {
    const res = await fetchJson(handle.port, "/sso/status", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as { exists: boolean; filePath: string; ageHours: number | null };
    expect(body.exists).toBe(false);
    expect(body.filePath).toBe(defaultSsoStatePath(runDir));
    expect(body.ageHours).toBeNull();
  });

  it("reports the file's mtime as ageHours when present", async () => {
    const path = defaultSsoStatePath(runDir);
    // The conventional `<runDir>/auth/` subdir doesn't exist yet on a
    // fresh tmpdir — create it before writing the captured state file.
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, JSON.stringify({ cookies: [], origins: [] }));
    const res = await fetchJson(handle.port, "/sso/status", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as {
      exists: boolean;
      size: number;
      ageHours: number;
      capturedAt: number;
    };
    expect(body.exists).toBe(true);
    expect(body.size).toBeGreaterThan(0);
    expect(body.ageHours).toBeGreaterThanOrEqual(0);
    expect(body.ageHours).toBeLessThan(0.001); // freshly written
    expect(body.capturedAt).toBeGreaterThan(0);
  });

  it("returns 503 when runDir was not supplied to the control server", async () => {
    await handle.close();
    handle = await startControlServer({ port: 0, token, surface: mockSurface() });
    const res = await fetchJson(handle.port, "/sso/status", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(503);
  });
});

describe("control server: SSO recapture lifecycle", () => {
  let handle: ControlServerHandle;
  let token: string;
  let runDir: string;
  let stub: ReturnType<typeof stubSession>;

  beforeEach(async () => {
    token = generateToken();
    runDir = mkdtempSync(join(tmpdir(), "bots-sso-"));
    stub = stubSession();
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      ssoCaptureFactory: async () => stub,
    });
  });

  afterEach(async () => {
    await handle.close();
    rmSync(runDir, { recursive: true, force: true });
  });

  it("spawn → complete writes the file and reports fresh status", async () => {
    const start = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(start.status).toBe(201);
    const startBody = start.body as { recaptureSessionId: string; startUrl: string };
    expect(startBody.recaptureSessionId).toMatch(/[0-9a-f-]+/);
    expect(startBody.startUrl).toMatch(/^https:\/\//);

    const done = await fetchJson(
      handle.port,
      `/sso/recapture/${startBody.recaptureSessionId}/complete`,
      { method: "POST", headers: { authorization: `Bearer ${token}` } },
    );
    expect(done.status).toBe(200);
    const doneBody = done.body as { exists: boolean; filePath: string };
    expect(doneBody.exists).toBe(true);
    expect(doneBody.filePath).toBe(defaultSsoStatePath(runDir));
    expect(stub.saved).toBe(true);
    expect(stub.savedPath).toBe(defaultSsoStatePath(runDir));
    expect(existsSync(defaultSsoStatePath(runDir))).toBe(true);
  });

  it("spawn → DELETE closes the browser and leaves no file behind", async () => {
    const filePath = defaultSsoStatePath(runDir);
    expect(existsSync(filePath)).toBe(false);
    const start = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    const id = (start.body as { recaptureSessionId: string }).recaptureSessionId;

    const del = await fetchJson(handle.port, `/sso/recapture/${id}`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(del.status).toBe(200);
    expect(stub.closed).toBe(true);
    expect(stub.saved).toBe(false);
    expect(existsSync(filePath)).toBe(false);
  });

  it("complete on an unknown session id returns 404", async () => {
    const res = await fetchJson(handle.port, `/sso/recapture/does-not-exist/complete`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("rejects an invalid startUrl", async () => {
    const res = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { startUrl: "ftp://nope" },
    });
    expect(res.status).toBe(400);
  });

  it("auto-cancels an idle session via the idle timer", async () => {
    // Replace the default idle timeout with a tiny one so we don't
    // have to use fake timers across an HTTP boundary. 50ms is enough
    // for the post-spawn response to land before the timer fires.
    await handle.close();
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      runDir,
      ssoCaptureFactory: async () => stub,
      ssoRecaptureIdleTimeoutMs: 50,
    });
    const start = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    const id = (start.body as { recaptureSessionId: string }).recaptureSessionId;
    await new Promise((res) => setTimeout(res, 120));
    // Server-side: the idle timer should have closed the session and
    // dropped the map entry. Attempting to complete it now 404s.
    const completeAfterTimeout = await fetchJson(handle.port, `/sso/recapture/${id}/complete`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(completeAfterTimeout.status).toBe(404);
    expect(stub.closed).toBe(true);
    expect(stub.saved).toBe(false);
  });

  it("closeSsoRecaptureSessions tears down stranded sessions", async () => {
    const start = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(start.status).toBe(201);
    expect(stub.closed).toBe(false);
    await handle.closeSsoRecaptureSessions();
    expect(stub.closed).toBe(true);
    expect(stub.saved).toBe(false);
  });

  it("returns 503 when runDir is missing", async () => {
    await handle.close();
    handle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
      ssoCaptureFactory: async () => stub,
    });
    const res = await fetchJson(handle.port, "/sso/recapture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(res.status).toBe(503);
  });
});

describe("control server: launch accepts ssoStateFile", () => {
  let handle: ControlServerHandle;
  let token: string;
  let runDir: string;
  let captured: Array<{ ssoStateFile?: string }> = [];

  beforeEach(async () => {
    token = generateToken();
    runDir = mkdtempSync(join(tmpdir(), "bots-sso-launch-"));
    captured = [];
    const surface: OrchestratorControlSurface = {
      ...mockSurface(),
      launchOne: async (spec) => {
        captured.push({ ssoStateFile: spec.ssoStateFile });
        return generateBotId();
      },
    };
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
    rmSync(runDir, { recursive: true, force: true });
  });

  it("forwards ssoStateFile from POST /launch to surface.launchOne", async () => {
    const ssoPath = "/tmp/some/hcl-sso.json";
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://app.videocall.fnxlabs.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        ssoStateFile: ssoPath,
      },
    });
    expect(res.status).toBe(201);
    expect(captured).toHaveLength(1);
    expect(captured[0].ssoStateFile).toBe(ssoPath);
  });

  it("rejects ssoStateFile that is not a string", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
        ssoStateFile: 42,
      },
    });
    expect(res.status).toBe(400);
  });
});
