import { describe, expect, it, vi } from "vitest";

import { readLogWindow, REMOTE_LOG_CAP, spawnRemoteBot } from "./ssh-launcher";
import type { SshHost } from "./ssh-hosts";

function host(overrides: Partial<SshHost> = {}): SshHost {
  return {
    label: "h1",
    host: "example.com",
    user: "alice",
    sshKey: null,
    reposPath: "/home/alice/videocall",
    notes: null,
    addedAt: 0,
    ...overrides,
  };
}

interface FakeChild {
  stdoutCb: ((b: Buffer) => void) | null;
  stderrCb: ((b: Buffer) => void) | null;
  exitCb: ((code: number | null) => void) | null;
  errorCb: ((err: Error) => void) | null;
  kill: ReturnType<typeof vi.fn>;
}

function stubSpawnFactory() {
  const last: { child: FakeChild | null } = { child: null };
  const fn = vi.fn().mockImplementation(() => {
    const child: FakeChild = {
      stdoutCb: null,
      stderrCb: null,
      exitCb: null,
      errorCb: null,
      kill: vi.fn(),
    };
    last.child = child;
    return {
      stdout: {
        on: (event: string, cb: (b: Buffer) => void) => {
          if (event === "data") child.stdoutCb = cb;
        },
      },
      stderr: {
        on: (event: string, cb: (b: Buffer) => void) => {
          if (event === "data") child.stderrCb = cb;
        },
      },
      on: (event: string, cb: (...args: unknown[]) => void) => {
        if (event === "exit") child.exitCb = cb as (code: number | null) => void;
        if (event === "error") child.errorCb = cb as (err: Error) => void;
      },
      kill: child.kill,
    };
  });
  return { fn, last };
}

describe("spawnRemoteBot", () => {
  it("invokes spawn('ssh', ...) with the host's argv", () => {
    const { fn } = stubSpawnFactory();
    spawnRemoteBot(
      {
        host: host({ host: "example.com:2222", sshKey: "/keys/id" }),
        ttl: "5m",
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
      },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    expect(fn).toHaveBeenCalledTimes(1);
    const [cmd, args] = fn.mock.calls[0];
    expect(cmd).toBe("ssh");
    expect(args).toContain("-i");
    expect(args).toContain("/keys/id");
    expect(args).toContain("-p");
    expect(args).toContain("2222");
    expect(args).toContain("alice@example.com");
    expect(args[args.length - 1]).toContain("npm run bot");
  });

  it("accumulates stdout lines in recentLog up to REMOTE_LOG_CAP", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      {
        host: host(),
        ttl: "5m",
        meetingURL: "u",
        participant: "p",
      },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    expect(last.child).not.toBeNull();
    // Push 250 lines; cap is 200.
    let combined = "";
    for (let i = 0; i < 250; i++) combined += `line ${i}\n`;
    last.child!.stdoutCb!(Buffer.from(combined, "utf8"));
    expect(handle.recentLog.length).toBe(REMOTE_LOG_CAP);
    expect(handle.totalLines).toBe(250);
    // The first preserved line should be the (250-200)=50th — that is "line 50".
    expect(handle.recentLog[0]).toBe("line 50");
    expect(handle.recentLog[REMOTE_LOG_CAP - 1]).toBe("line 249");
  });

  it("resolves exit with the process code and marks finished", async () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      {
        host: host(),
        ttl: "5m",
        meetingURL: "u",
        participant: "p",
      },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    expect(last.child).not.toBeNull();
    last.child!.exitCb!(0);
    const code = await handle.exit;
    expect(code).toBe(0);
    expect(handle.finished).toBe(true);
    expect(handle.finishReason).toBe("remote-exit-ok");
  });

  it("marks exit as remote-exit-error for nonzero exit codes", async () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.exitCb!(255);
    await handle.exit;
    expect(handle.finishReason).toBe("remote-exit-error");
    expect(handle.exitCode).toBe(255);
  });

  it("treats spawn errors as ssh-error", async () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.errorCb!(new Error("ENOENT: no such file"));
    await handle.exit;
    expect(handle.finishReason).toBe("ssh-error");
    expect(handle.recentLog.join("\n")).toContain("spawn error: ENOENT");
  });
});

describe("readLogWindow", () => {
  it("returns all current buffer lines when since=0", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.stdoutCb!(Buffer.from("a\nb\nc\n", "utf8"));
    const window = readLogWindow(handle, 0);
    expect(window.lines).toEqual(["a", "b", "c"]);
    expect(window.totalLines).toBe(3);
  });

  it("returns only the tail when since is mid-stream", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.stdoutCb!(Buffer.from("a\nb\nc\nd\n", "utf8"));
    const window = readLogWindow(handle, 2);
    expect(window.lines).toEqual(["c", "d"]);
    expect(window.totalLines).toBe(4);
  });

  it("returns an empty array when since >= totalLines", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.stdoutCb!(Buffer.from("a\nb\n", "utf8"));
    const window = readLogWindow(handle, 99);
    expect(window.lines).toEqual([]);
    expect(window.totalLines).toBe(2);
  });
});
