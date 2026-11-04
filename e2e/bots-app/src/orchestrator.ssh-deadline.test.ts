import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, onTestFinished, vi } from "vitest";

import type { SshBotHandle, SshLaunchSpec } from "./control/ssh-launcher";

const mocks = vi.hoisted(() => ({ spawnRemoteBot: vi.fn() }));
vi.mock("./control/ssh-launcher", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./control/ssh-launcher")>()),
  spawnRemoteBot: mocks.spawnRemoteBot,
}));

import { addHost } from "./control/ssh-hosts";
import { runBotsToCompletion } from "./orchestrator";

describe("control SSH launches and the run deadline", () => {
  it("passes the run deadline to the remote bot", async () => {
    const seen: Array<number | null | undefined> = [];
    mocks.spawnRemoteBot.mockImplementation((spec: SshLaunchSpec): SshBotHandle => {
      seen.push(spec.deadlineSecs);
      return {
        host: spec.host,
        child: { kill: () => true } as unknown as SshBotHandle["child"],
        recentLog: [],
        totalLines: 0,
        exitCode: null,
        finished: false,
        exit: Promise.resolve(0),
      };
    });
    const runDir = mkdtempSync(join(tmpdir(), "bots-app-ssh-deadline-"));
    onTestFinished(() => rmSync(runDir, { recursive: true, force: true }));
    await addHost(runDir, {
      label: "box",
      host: "box.invalid",
      user: "bot",
      reposPath: "/repos/videocall",
    });
    const deadline = Date.now() + 1_500;
    let port = 0;
    const run = runBotsToCompletion({
      tasks: [],
      runDeadlineMs: deadline,
      control: {
        port: 0,
        token: "test-token",
        tokenFilePath: join(runDir, "ctl.json"),
        runDir,
        onListen: async (info) => {
          port = info.port;
        },
      },
    });
    while (port === 0) await new Promise((r) => setTimeout(r, 10));
    const res = await fetch(`http://127.0.0.1:${port}/launch`, {
      method: "POST",
      headers: { authorization: "Bearer test-token", "content-type": "application/json" },
      body: JSON.stringify({
        meetingURL: "https://example.invalid/meeting/X",
        participant: "remote",
        ttl: "1m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        videoMode: "clock",
        runLocation: { kind: "ssh", hostLabel: "box" },
      }),
    });
    await run;
    expect(res.status).toBe(201);
    expect(seen).toEqual([Math.floor(deadline / 1000)]);
  }, 10_000);
});
