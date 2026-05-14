import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { listAssetFiles, resolveCtlConfig, startDashboardServer } from "./dashboard";
import { defaultTokenFilePath, generateToken, writeTokenFile } from "./control/auth";
import {
  startControlServer,
  type ControlServerHandle,
  type LaunchSpec,
  type OrchestratorControlSurface,
} from "./control/server";
import type { BotRegistryEntry } from "./control/registry";

function emptySurface(): OrchestratorControlSurface {
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
    duplicateBot: async () => "00000000-0000-0000-0000-000000000001",
    launchOne: async (_spec: LaunchSpec) => "00000000-0000-0000-0000-000000000002",
  };
}

describe("listAssetFiles", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "bots-dashboard-"));
  });

  it("returns sorted basenames of files matching the allowed extensions", () => {
    writeFileSync(join(dir, "carol.wav"), "");
    writeFileSync(join(dir, "alice.wav"), "");
    writeFileSync(join(dir, "_silence.wav"), "");
    writeFileSync(join(dir, "readme.txt"), "");
    expect(listAssetFiles(dir, [".wav"])).toEqual(["alice.wav", "carol.wav"]);
  });

  it("returns an empty array when the directory is missing", () => {
    expect(listAssetFiles(join(dir, "nope"), [".wav"])).toEqual([]);
  });
});

describe("resolveCtlConfig", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "bots-dashboard-"));
  });

  it("auto-discovers a token file under runDir", async () => {
    const path = defaultTokenFilePath(dir, 999);
    await writeTokenFile(path, {
      port: 9876,
      token: "a".repeat(64),
      startedAt: new Date().toISOString(),
      pid: 999,
    });
    const cfg = await resolveCtlConfig({ runDir: dir });
    expect(cfg.port).toBe(9876);
    expect(cfg.token).toBe("a".repeat(64));
  });

  it("throws when no token file is found", async () => {
    await expect(resolveCtlConfig({ runDir: dir })).rejects.toThrow(/no ctl token file/);
  });

  it("accepts explicit --ctl-port + --ctl-token", async () => {
    const cfg = await resolveCtlConfig({ runDir: dir, port: 9999, token: "abc" });
    expect(cfg.port).toBe(9999);
    expect(cfg.token).toBe("abc");
  });

  it("rejects --ctl-port without --ctl-token", async () => {
    await expect(resolveCtlConfig({ runDir: dir, port: 9999 })).rejects.toThrow(
      /supplied together/,
    );
  });
});

describe("dashboard HTTP server", () => {
  let dashboard: { port: number; close(): Promise<void> } | null = null;
  let ctlHandle: ControlServerHandle | null = null;
  let dir: string;
  let token: string;

  beforeEach(async () => {
    token = generateToken();
    dir = mkdtempSync(join(tmpdir(), "bots-dashboard-"));
    mkdirSync(join(dir, "audio"));
    mkdirSync(join(dir, "costumes"));
    writeFileSync(join(dir, "audio", "alice.wav"), "");
    writeFileSync(join(dir, "costumes", "cat.y4m"), "");
    ctlHandle = await startControlServer({ port: 0, token, surface: emptySurface() });
    const handle = await startDashboardServer({
      port: 0,
      ctl: { port: ctlHandle.port, token },
      assetsDir: dir,
    });
    dashboard = handle;
  });

  afterEach(async () => {
    await dashboard?.close();
    if (ctlHandle) {
      // The 502-when-unreachable test closes the ctl handle inside
      // the body and nulls it out; tolerate "already closed" here.
      await ctlHandle.close().catch(() => {});
    }
  });

  it("synthesizes /api/daemon locally without hitting the ctl API", async () => {
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/daemon`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { port: number };
    expect(body.port).toBe(ctlHandle!.port);
  });

  it("serves /api/assets/audio from the asset directory", async () => {
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/assets/audio`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { files: string[] };
    expect(body.files).toEqual(["alice.wav"]);
  });

  it("serves /api/assets/costumes from the asset directory", async () => {
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/assets/costumes`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { files: string[] };
    expect(body.files).toEqual(["cat.y4m"]);
  });

  it("proxies /api/healthz to the ctl API (no token leaked to browser)", async () => {
    // No Authorization header on the inbound request — the dashboard
    // server injects the bearer token before forwarding to the ctl API.
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/healthz`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { ok: boolean };
    expect(body.ok).toBe(true);
  });

  it("proxies /api/launch and the ctl API sees the bearer token", async () => {
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/launch`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: false,
        network: "none",
        authBackend: "jwt",
      }),
    });
    expect(res.status).toBe(201);
    const body = (await res.json()) as { botId: string };
    expect(body.botId).toMatch(/^[0-9a-f-]+$/);
  });

  it("returns 502 when the ctl API is unreachable", async () => {
    // Tear the ctl handle down so the proxy attempt fails. The
    // dashboard must surface a 502 rather than crashing.
    await ctlHandle!.close();
    ctlHandle = null;
    const res = await fetch(`http://127.0.0.1:${dashboard!.port}/api/healthz`);
    expect(res.status).toBe(502);
  });
});
