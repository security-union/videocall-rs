import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { SsoCaptureSession } from "../auth/sso-capture";
import { generateToken } from "./auth";
import { generateBotId } from "./registry";
import { type ControlServerHandle, startControlServer } from "./server";

const FORGED = "[control] FORGED-BY-ERROR-MESSAGE";

const surface = {
  getRegistry: () => new Map(),
  expectedBots: () => 0,
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

const failingSession = async (): Promise<SsoCaptureSession> => ({
  browser: {} as never,
  context: {} as never,
  saveAndClose: async () => {},
  close: async () => {
    throw new Error(`boom\n${FORGED}`);
  },
});

let handle: ControlServerHandle | null = null;
let runDir: string | null = null;

afterEach(async () => {
  vi.restoreAllMocks();
  await handle?.close();
  handle = null;
  if (runDir) rmSync(runDir, { recursive: true, force: true });
  runDir = null;
});

/** Start a server, open one capture on `route`, run `after`, return what was written. */
async function linesFrom(
  route: "/sso/recapture" | "/oauth/capture",
  after: (h: ControlServerHandle) => Promise<void>,
  idleTimeoutMs?: number,
): Promise<string[]> {
  const written: string[] = [];
  const sink = (...args: unknown[]): void => {
    written.push(args.map(String).join(" "));
  };
  for (const m of ["log", "warn", "error"] as const) {
    vi.spyOn(console, m).mockImplementation(sink);
  }
  const token = generateToken();
  runDir = mkdtempSync(join(tmpdir(), "control-logline-"));
  handle = await startControlServer({
    port: 0,
    token,
    surface,
    runDir,
    ssoCaptureFactory: failingSession,
    ssoRecaptureIdleTimeoutMs: idleTimeoutMs,
  });
  const res = await fetch(`http://127.0.0.1:${handle.port}${route}`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ label: "alice" }),
  });
  expect(res.status).toBe(201);
  await after(handle);
  return written.join("\n").split(/[\r\n]/);
}

function expectCollapsed(lines: string[], site: string): void {
  expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
  expect(lines.filter((l) => l.includes(site) && l.includes(FORGED))).toHaveLength(1);
}

const idle = async (): Promise<void> => {
  await new Promise((r) => setTimeout(r, 150));
};

describe("[control] writers collapse an interpolated error message (#2484)", () => {
  it("stranded sso recapture close", async () => {
    const lines = await linesFrom("/sso/recapture", (h) => h.closeSsoRecaptureSessions());
    expectCollapsed(lines, "failed to close stranded sso recapture");
  });

  it("stranded oauth capture close", async () => {
    const lines = await linesFrom("/oauth/capture", (h) => h.closeSsoRecaptureSessions());
    expectCollapsed(lines, "failed to close stranded oauth capture");
  });

  it("idle-timeout teardown of an sso recapture", async () => {
    const lines = await linesFrom("/sso/recapture", idle, 50);
    expectCollapsed(lines, "idle-timeout teardown of sso recapture");
    expect(lines.some((l) => l.startsWith("[control] sso recapture "))).toBe(true);
  });

  it("idle-timeout teardown of an oauth capture", async () => {
    const lines = await linesFrom("/oauth/capture", idle, 50);
    expectCollapsed(lines, "idle-timeout teardown of oauth capture");
    expect(lines.some((l) => l.startsWith("[control] oauth capture "))).toBe(true);
  });
});
