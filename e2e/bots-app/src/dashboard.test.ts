import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, type Server } from "node:http";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  crossOriginRejection,
  isLoopbackOrigin,
  listAssetFiles,
  resolveCtlConfig,
  startDashboardServer,
  type DashboardServerHandle,
} from "./dashboard";
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
    const body = (await res.json()) as { port: number; mode: string };
    expect(body.port).toBe(ctlHandle!.port);
    // Default is "attached" unless the CLI / caller explicitly opts in.
    expect(body.mode).toBe("attached");
  });

  it("reports daemonMode=self-hosted when the option is set", async () => {
    const localToken = generateToken();
    const localDir = mkdtempSync(join(tmpdir(), "bots-dashboard-mode-"));
    const localCtl = await startControlServer({
      port: 0,
      token: localToken,
      surface: emptySurface(),
    });
    const localDash = await startDashboardServer({
      port: 0,
      ctl: { port: localCtl.port, token: localToken },
      assetsDir: localDir,
      daemonMode: "self-hosted",
    });
    try {
      const res = await fetch(`http://127.0.0.1:${localDash.port}/api/daemon`);
      const body = (await res.json()) as { mode: string };
      expect(body.mode).toBe("self-hosted");
    } finally {
      await localDash.close();
      await localCtl.close();
    }
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

/**
 * Regression lock for #2211. The 403 alone proves nothing, so every rejection also
 * asserts ctl recorded NO request — a "forward, then 403" ordering still fires the
 * launch.
 */
describe("dashboard /api CSRF gate", () => {
  interface ForwardedRequest {
    method: string;
    url: string;
    authorization: string | undefined;
  }

  let upstream: Server | null = null;
  let dashboard: DashboardServerHandle | null = null;
  let forwarded: ForwardedRequest[] = [];

  beforeEach(async () => {
    forwarded = [];
    const server = createServer((req, res) => {
      forwarded.push({
        method: req.method ?? "",
        url: req.url ?? "",
        authorization: req.headers["authorization"],
      });
      req.resume();
      res.writeHead(200, { "content-type": "application/json; charset=utf-8" });
      res.end(JSON.stringify({ ok: true }));
    });
    upstream = server;
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const addr = server.address();
    if (addr === null || typeof addr === "string") throw new Error("no upstream port");
    dashboard = await startDashboardServer({
      port: 0,
      ctl: { port: addr.port, token: "ctl-token" },
      assetsDir: "/nonexistent",
    });
  });

  afterEach(async () => {
    if (dashboard) {
      dashboard.server.closeAllConnections();
      await dashboard.close();
      dashboard = null;
    }
    if (upstream) {
      upstream.closeAllConnections();
      const server = upstream;
      await new Promise<void>((r) => server.close(() => r()));
      upstream = null;
    }
  });

  function call(path: string, headers: Record<string, string>, method = "GET"): Promise<Response> {
    return fetch(`http://127.0.0.1:${dashboard!.port}${path}`, { method, headers });
  }

  it("rejects a foreign Origin on a state-changing route without forwarding it", async () => {
    const res = await call(
      "/api/launch",
      { origin: "https://evil.attacker.test", "sec-fetch-site": "cross-site" },
      "POST",
    );
    expect(res.status).toBe(403);
    expect(((await res.json()) as { error: string }).error).toContain("cross-origin");
    expect(forwarded).toEqual([]);

    // Positive control: the recorder is live, so the empty array above is real.
    const allowed = await call(
      "/api/launch",
      { origin: `http://127.0.0.1:${dashboard!.port}`, "sec-fetch-site": "same-origin" },
      "POST",
    );
    expect(allowed.status).toBe(200);
    expect(forwarded).toHaveLength(1);
    expect(forwarded[0].authorization).toBe("Bearer ctl-token");
  });

  it("rejects a cross-site GET that carries no Origin at all", async () => {
    // The `<img src>` shape — no-cors GET carries no Origin, so an Origin-only
    // guard forwards it.
    const res = await call("/api/bots", { "sec-fetch-site": "cross-site" });
    expect(res.status).toBe(403);
    expect(forwarded).toEqual([]);
  });

  it.each(["cross-site", "same-site", "unexpected-future-value"])(
    "rejects sec-fetch-site: %s without forwarding",
    async (site) => {
      const res = await call("/api/bots", { "sec-fetch-site": site });
      expect(res.status).toBe(403);
      expect(forwarded).toEqual([]);
    },
  );

  it("rejects a foreign Origin even when Sec-Fetch-Site claims same-origin", async () => {
    // The two gates are independent; a browser cannot emit this pair.
    const res = await call("/api/bots", {
      origin: "https://evil.attacker.test",
      "sec-fetch-site": "same-origin",
    });
    expect(res.status).toBe(403);
    expect(forwarded).toEqual([]);
  });

  it("rejects the opaque null Origin of a sandboxed frame", async () => {
    const res = await call("/api/bots", { origin: "null" });
    expect(res.status).toBe(403);
    expect(forwarded).toEqual([]);
  });

  it("gates the locally-synthesized endpoints too, not just the proxy", async () => {
    for (const path of ["/api/daemon", "/api/assets/audio", "/api/assets/costumes"]) {
      const res = await call(path, { origin: "https://evil.attacker.test" });
      expect(res.status).toBe(403);
    }
    expect(forwarded).toEqual([]);
  });

  it("forwards the built-mode same-origin shape", async () => {
    const res = await call("/api/bots", {
      origin: `http://127.0.0.1:${dashboard!.port}`,
      "sec-fetch-site": "same-origin",
    });
    expect(res.status).toBe(200);
    expect(forwarded).toHaveLength(1);
  });

  it("forwards the Vite dev shape, whose Origin is a different loopback port", async () => {
    // `changeOrigin: true` rewrites Host but not Origin — a Host-vs-Origin
    // comparison would 403 the operator's own dev server.
    const res = await call("/api/bots", {
      origin: "http://localhost:5173",
      "sec-fetch-site": "same-origin",
    });
    expect(res.status).toBe(200);
    expect(forwarded).toHaveLength(1);
  });

  it("forwards a caller that sends neither header", async () => {
    const res = await call("/api/bots", {});
    expect(res.status).toBe(200);
    expect(forwarded).toHaveLength(1);
  });

  it("forwards a user-initiated load (sec-fetch-site: none)", async () => {
    const res = await call("/api/bots", { "sec-fetch-site": "none" });
    expect(res.status).toBe(200);
    expect(forwarded).toHaveLength(1);
  });

  it("does not gate static serving — a cross-site link to the page must open", async () => {
    const res = await call("/", { "sec-fetch-site": "cross-site" });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/html");
  });
});

describe("crossOriginRejection", () => {
  it("allows a request with neither signal", () => {
    expect(crossOriginRejection({})).toBeNull();
  });

  it.each(["same-origin", "none", "SAME-ORIGIN"])("allows sec-fetch-site %s", (site) => {
    expect(crossOriginRejection({ "sec-fetch-site": site })).toBeNull();
  });

  it.each(["cross-site", "same-site"])("names sec-fetch-site %s in the reason", (site) => {
    expect(crossOriginRejection({ "sec-fetch-site": site })).toBe(`sec-fetch-site: ${site}`);
  });

  it.each(["", " "])("treats a valueless header (%s) as absent, not as a claim", (site) => {
    expect(crossOriginRejection({ "sec-fetch-site": site, origin: site })).toBeNull();
  });

  it("reports the offending origin so the 403 is diagnosable", () => {
    expect(crossOriginRejection({ origin: "https://evil.attacker.test" })).toBe(
      "origin: https://evil.attacker.test",
    );
  });

  it("fails closed on the comma-joined string Node builds from a duplicated header", () => {
    // Smuggling a loopback origin alongside a foreign one must not buy an allow.
    expect(crossOriginRejection({ origin: "https://evil.attacker.test, http://127.0.0.1" })).toBe(
      "origin: https://evil.attacker.test, http://127.0.0.1",
    );
    expect(crossOriginRejection({ origin: "http://127.0.0.1, https://evil.attacker.test" })).toBe(
      "origin: http://127.0.0.1, https://evil.attacker.test",
    );
    expect(crossOriginRejection({ "sec-fetch-site": "cross-site, same-origin" })).toBe(
      "sec-fetch-site: cross-site, same-origin",
    );
  });
});

describe("isLoopbackOrigin", () => {
  it.each([
    "http://127.0.0.1:5174",
    "http://127.0.0.1",
    "http://127.1.2.3:8080",
    "http://localhost:5173",
    "https://localhost",
    "http://[::1]:5174",
    // Both normalize before the hostname set is consulted.
    "http://[0:0:0:0:0:0:0:1]:5174",
    "http://[::ffff:127.0.0.1]",
  ])("accepts %s", (origin) => {
    expect(isLoopbackOrigin(origin)).toBe(true);
  });

  it.each([
    "https://evil.attacker.test",
    "http://localhost.evil.test",
    "http://127.0.0.1.evil.test",
    "http://evil.test#127.0.0.1",
    "null",
    "",
    "file://",
    "chrome-extension://abcdef",
  ])("rejects %s", (origin) => {
    expect(isLoopbackOrigin(origin)).toBe(false);
  });
});
