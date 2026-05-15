import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { generateMeetingConfig, emitMeetingConfigYaml } from "../meeting-config";
import { loadManifest } from "../manifest";
import { generateToken } from "./auth";
import {
  type ControlServerHandle,
  type LaunchSpec,
  type OrchestratorControlSurface,
  pickParticipantsForMultiLaunch,
  startControlServer,
} from "./server";
import { type BotRegistryEntry, generateBotId } from "./registry";

const SAMPLE_MANIFEST_YAML = `pause_ms: 250
participants:
  - name: alice
    costume_dir: assets/costumes/pirate
  - name: bob
    costume_dir: assets/costumes/ninja
  - name: carol
    costume_dir: assets/costumes/astronaut
  - name: dave
    costume_dir: assets/costumes/wizard
  - name: erin
  - name: observer-01
lines:
  - speaker: alice
    audio_file: audio/alice-1.wav
  - speaker: bob
    audio_file: audio/bob-1.wav
`;

interface MockSurface extends OrchestratorControlSurface {
  registry: Map<string, BotRegistryEntry>;
  launchSpecs: LaunchSpec[];
  failNextNLaunches: number;
}

function mockSurface(): MockSurface {
  const registry = new Map<string, BotRegistryEntry>();
  const launchSpecs: LaunchSpec[] = [];
  return {
    registry,
    launchSpecs,
    failNextNLaunches: 0,
    getRegistry: () => registry,
    triggerLeave: async () => {},
    forceKill: async () => {},
    applyTtl: () => {},
    changeNetwork: async () => {},
    setMicMuted: async () => {},
    setCameraOff: async () => {},
    setScreenShare: async () => {},
    duplicateBot: async () => generateBotId(),
    launchOne: async function (spec: LaunchSpec) {
      const self = this as MockSurface;
      if (self.failNextNLaunches > 0) {
        self.failNextNLaunches -= 1;
        throw new Error("simulated launch failure");
      }
      self.launchSpecs.push(spec);
      return generateBotId();
    },
  } as MockSurface;
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

describe("multi-launch picker", () => {
  it("first-n picks manifest order", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    const picked = pickParticipantsForMultiLaunch({
      manifest,
      mode: "first-n",
      count: 3,
    });
    expect(picked).toEqual(["alice", "bob", "carol"]);
  });

  it("first-n throws when count exceeds manifest participants", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    expect(() => pickParticipantsForMultiLaunch({ manifest, mode: "first-n", count: 100 })).toThrow(
      /exceeds the manifest/,
    );
  });

  it("random pick is deterministic given the same seed", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    const a = pickParticipantsForMultiLaunch({
      manifest,
      mode: "random",
      count: 3,
      seed: 42,
    });
    const b = pickParticipantsForMultiLaunch({
      manifest,
      mode: "random",
      count: 3,
      seed: 42,
    });
    expect(a).toEqual(b);
    expect(a).toHaveLength(3);
    // erin has no costume_dir, observer-01 has no costume_dir, so they
    // must not appear in a default (costumed-only) pick.
    expect(a).not.toContain("erin");
    expect(a).not.toContain("observer-01");
  });

  it("random pick mirrors generateMeetingConfig's RNG order", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    const seed = 1234;
    const count = 3;
    const cfg = generateMeetingConfig({
      manifest,
      count,
      seed,
      meetingUrl: "https://example.com/meeting/X",
    });
    const direct = pickParticipantsForMultiLaunch({
      manifest,
      mode: "random",
      count,
      seed,
    });
    expect(direct).toEqual(cfg.bots.map((b) => b.participant));
  });

  it("random includeObservers expands the eligible pool", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    const seed = 7;
    const picked = pickParticipantsForMultiLaunch({
      manifest,
      mode: "random",
      count: manifest.participants.length,
      seed,
      includeObservers: true,
    });
    // With every slot eligible the result must be a permutation of all
    // participants — including the observer.
    expect(picked).toContain("observer-01");
    expect(new Set(picked).size).toEqual(manifest.participants.length);
  });

  it("random throws when count exceeds eligible (costumed-only) pool", () => {
    const dir = mkdtempSync(join(tmpdir(), "multi-launch-pick-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    // 4 costumed participants (alice, bob, carol, dave); 5 should fail.
    expect(() =>
      pickParticipantsForMultiLaunch({ manifest, mode: "random", count: 5, seed: 1 }),
    ).toThrow(/exceeds the manifest/);
  });
});

describe("POST /launch/multi", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;
  let manifestPath: string;

  beforeEach(async () => {
    runDir = mkdtempSync(join(tmpdir(), "multi-launch-server-"));
    manifestPath = join(runDir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    token = generateToken();
    surface = mockSurface();
    handle = await startControlServer({
      port: 0,
      token,
      surface,
      runDir,
      manifestPath,
    });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("first-n mode spawns bots in manifest order", async () => {
    const res = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "first-n",
        count: 2,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    expect(res.status).toBe(202);
    const body = res.body as {
      botIds: string[];
      participants: string[];
      errors: unknown[];
    };
    expect(body.participants).toEqual(["alice", "bob"]);
    expect(body.botIds).toHaveLength(2);
    expect(body.errors).toEqual([]);
    expect(surface.launchSpecs.map((s) => s.participant)).toEqual(["alice", "bob"]);
    expect(surface.launchSpecs[0].ttl).toBe(300_000);
  });

  it("random mode is reproducible given a seed", async () => {
    const first = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "random",
        count: 2,
        seed: 99,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    const second = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "random",
        count: 2,
        seed: 99,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    const a = (first.body as { participants: string[] }).participants;
    const b = (second.body as { participants: string[] }).participants;
    expect(a).toEqual(b);
  });

  it("rejects count > maxUsers with 400", async () => {
    const res = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "first-n",
        count: 4,
        maxUsers: 3,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    expect(res.status).toBe(400);
    expect((res.body as { error: string }).error).toMatch(/maxUsers/);
  });

  it("rejects count > eligible pool with 400 (random, costumed-only)", async () => {
    const res = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "random",
        count: 5,
        seed: 1,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    expect(res.status).toBe(400);
    expect((res.body as { error: string }).error).toMatch(/exceeds the manifest/);
  });

  it("applies the displayNameTemplate to each spawned bot", async () => {
    await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "first-n",
        count: 2,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
        displayNameTemplate: "Bot {participant}",
      },
    });
    expect(surface.launchSpecs[0].displayName).toBe("Bot alice");
    expect(surface.launchSpecs[1].displayName).toBe("Bot bob");
  });

  it("collects errors mid-batch but keeps already-spawned bots", async () => {
    surface.failNextNLaunches = 1; // fail the first one only
    const res = await fetchJson(handle.port, "/launch/multi", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        mode: "first-n",
        count: 2,
        meetingURL: "https://example.com/meeting/X",
        ttl: "5m",
      },
    });
    expect(res.status).toBe(202);
    const body = res.body as { botIds: string[]; errors: Array<{ participant: string }> };
    expect(body.botIds).toHaveLength(1);
    expect(body.errors).toHaveLength(1);
    expect(body.errors[0].participant).toBe("alice");
  });
});

describe("POST /launch/from-config", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    runDir = mkdtempSync(join(tmpdir(), "from-config-"));
    token = generateToken();
    surface = mockSurface();
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("parses a valid YAML and spawns one bot per entry", async () => {
    const yaml =
      "meeting_url: https://example.com/meeting/X\n" +
      "ttl: 10m\n" +
      "bots:\n" +
      "  - participant: alice\n" +
      "  - participant: bob\n" +
      "    ttl: 30s\n";
    const res = await fetchJson(handle.port, "/launch/from-config", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { configYaml: yaml },
    });
    expect(res.status).toBe(202);
    const body = res.body as { count: number; botIds: string[]; errors: unknown[] };
    expect(body.count).toBe(2);
    expect(body.errors).toEqual([]);
    expect(surface.launchSpecs[0].participant).toBe("alice");
    expect(surface.launchSpecs[0].ttl).toBe(600_000);
    expect(surface.launchSpecs[1].participant).toBe("bob");
    expect(surface.launchSpecs[1].ttl).toBe(30_000);
  });

  it("rejects malformed YAML with 400 and the parser error message", async () => {
    const res = await fetchJson(handle.port, "/launch/from-config", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { configYaml: "not a real config" },
    });
    expect(res.status).toBe(400);
    const err = (res.body as { error: string }).error;
    expect(err).toMatch(/meeting config parse failed/i);
  });

  it("rejects missing configYaml with 400", async () => {
    const res = await fetchJson(handle.port, "/launch/from-config", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {},
    });
    expect(res.status).toBe(400);
  });

  it("preview endpoint returns parsed config without launching", async () => {
    const dir = mkdtempSync(join(tmpdir(), "from-config-preview-"));
    const manifestPath = join(dir, "manifest.yaml");
    writeFileSync(manifestPath, SAMPLE_MANIFEST_YAML);
    const { manifest } = loadManifest(manifestPath);
    const cfg = generateMeetingConfig({
      manifest,
      count: 3,
      seed: 12,
      meetingUrl: "https://example.com/meeting/X",
      ttl: "5m",
    });
    const yaml = emitMeetingConfigYaml(cfg);
    const res = await fetchJson(handle.port, "/launch/from-config/preview", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { configYaml: yaml },
    });
    expect(res.status).toBe(200);
    const body = res.body as { botCount: number; bots: unknown[] };
    expect(body.botCount).toBe(3);
    expect(body.bots).toHaveLength(3);
    // The launch surface must not have been touched by preview.
    expect(surface.launchSpecs).toHaveLength(0);
  });
});

describe("OAuth session endpoints", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;
  let authDir: string;

  beforeEach(async () => {
    runDir = mkdtempSync(join(tmpdir(), "oauth-sessions-"));
    authDir = join(runDir, "auth");
    token = generateToken();
    surface = mockSurface();
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("lists captured sessions, excluding hcl-sso.json", async () => {
    const { mkdirSync } = await import("node:fs");
    mkdirSync(authDir, { recursive: true });
    writeFileSync(join(authDir, "alice.json"), "{}");
    writeFileSync(join(authDir, "bob.json"), "{}");
    writeFileSync(join(authDir, "hcl-sso.json"), "{}");
    const res = await fetchJson(handle.port, "/oauth/sessions", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    const body = res.body as { sessions: Array<{ label: string }> };
    const labels = body.sessions.map((s) => s.label);
    expect(labels.sort()).toEqual(["alice", "bob"]);
  });

  it("rejects a label with invalid characters on capture start", async () => {
    const res = await fetchJson(handle.port, "/oauth/capture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { label: "../etc/passwd" },
    });
    expect(res.status).toBe(400);
  });

  it("rejects a path-traversal session label on delete", async () => {
    const res = await fetchJson(handle.port, `/oauth/sessions/${encodeURIComponent("../bad")}`, {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(400);
  });

  it("deletes an existing session file", async () => {
    const { mkdirSync } = await import("node:fs");
    mkdirSync(authDir, { recursive: true });
    writeFileSync(join(authDir, "alice.json"), "{}");
    const res = await fetchJson(handle.port, "/oauth/sessions/alice", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect((res.body as { deleted: boolean }).deleted).toBe(true);
  });

  it("returns 404 when deleting a non-existent label", async () => {
    const res = await fetchJson(handle.port, "/oauth/sessions/nope", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("captures + saves a session via mocked Playwright factory", async () => {
    // Replace the handle with one configured to use a mock factory.
    await handle.close();
    let savedPath: string | null = null;
    const closed = { value: false };
    const mockFactory = async (): Promise<import("../auth/sso-capture").SsoCaptureSession> => ({
      // Stubs that satisfy the SsoCaptureSession contract.
      browser: {} as unknown as import("@playwright/test").Browser,
      context: {} as unknown as import("@playwright/test").BrowserContext,
      saveAndClose: async (path: string) => {
        savedPath = path;
        const { mkdirSync } = await import("node:fs");
        mkdirSync(join(authDir), { recursive: true });
        writeFileSync(path, JSON.stringify({ cookies: [], origins: [] }));
        closed.value = true;
      },
      close: async () => {
        closed.value = true;
      },
    });
    handle = await startControlServer({
      port: 0,
      token,
      surface,
      runDir,
      ssoCaptureFactory: mockFactory,
    });
    const start = await fetchJson(handle.port, "/oauth/capture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { label: "alice", startUrl: "https://app.videocall.rs/" },
    });
    expect(start.status).toBe(201);
    const sessionId = (start.body as { captureSessionId: string }).captureSessionId;
    const complete = await fetchJson(handle.port, `/oauth/capture/${sessionId}/complete`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(complete.status).toBe(200);
    expect((complete.body as { label: string }).label).toBe("alice");
    expect(savedPath).toBe(join(authDir, "alice.json"));
    expect(closed.value).toBe(true);
  });

  it("returns 404 when completing an unknown capture session", async () => {
    const res = await fetchJson(handle.port, "/oauth/capture/does-not-exist/complete", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("idle-timeout auto-cancels an abandoned capture", async () => {
    await handle.close();
    let closeCalls = 0;
    const mockFactory = async (): Promise<import("../auth/sso-capture").SsoCaptureSession> => ({
      browser: {} as unknown as import("@playwright/test").Browser,
      context: {} as unknown as import("@playwright/test").BrowserContext,
      saveAndClose: async () => {},
      close: async () => {
        closeCalls += 1;
      },
    });
    handle = await startControlServer({
      port: 0,
      token,
      surface,
      runDir,
      ssoCaptureFactory: mockFactory,
      ssoRecaptureIdleTimeoutMs: 50,
    });
    const start = await fetchJson(handle.port, "/oauth/capture", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: { label: "alice" },
    });
    expect(start.status).toBe(201);
    // Wait past the idle timeout. The timer is unref'd but still fires.
    await new Promise((r) => setTimeout(r, 150));
    expect(closeCalls).toBeGreaterThanOrEqual(1);
    // A follow-up complete should return 404.
    const sessionId = (start.body as { captureSessionId: string }).captureSessionId;
    const complete = await fetchJson(handle.port, `/oauth/capture/${sessionId}/complete`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(complete.status).toBe(404);
  });
});
