import { describe, expect, it, vi } from "vitest";

import { buildSshCommand, readLogWindow, REMOTE_LOG_CAP, spawnRemoteBot } from "./ssh-launcher";
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
    // The launcher emits the SSH command as line 1 (`$ ssh …`) so the
    // total line count starts at 1, not 0. Push 250 stdout lines on top
    // → 251 total, capped at 200.
    expect(handle.totalLines).toBe(1);
    expect(handle.recentLog[0]).toMatch(/^\$ ssh /);
    let combined = "";
    for (let i = 0; i < 250; i++) combined += `line ${i}\n`;
    last.child!.stdoutCb!(Buffer.from(combined, "utf8"));
    expect(handle.recentLog.length).toBe(REMOTE_LOG_CAP);
    expect(handle.totalLines).toBe(251);
    // pushLine trims after each append once length exceeds the cap. With
    // the header at index 0 + 250 lines pushed sequentially, the trim
    // drops the header on the 200th push, then drops "line 0"…"line 49"
    // on the next 50. First preserved is "line 50", last is "line 249".
    expect(handle.recentLog[0]).toBe("line 50");
    expect(handle.recentLog[REMOTE_LOG_CAP - 1]).toBe("line 249");
  });

  it("records the executed SSH command as the first line of recentLog", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      {
        host: host({ host: "example.com:2222", sshKey: "/keys/id" }),
        ttl: "5m",
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
      },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    expect(last.child).not.toBeNull();
    expect(handle.recentLog.length).toBe(1);
    expect(handle.totalLines).toBe(1);
    const firstLine = handle.recentLog[0];
    expect(firstLine.startsWith("$ ssh ")).toBe(true);
    expect(firstLine).toContain("alice@example.com");
    expect(firstLine).toContain("ConnectTimeout=10");
    expect(firstLine).toContain("npm run bot");
    // The remote command lives inside a single-quoted argv slot, so the
    // inner `--participant 'alice'` shows up with the standard '\\'' dance.
    expect(firstLine).toContain("--participant '\\''alice'\\''");
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
  // Each test pushes its stdout after the command-header line that
  // spawnRemoteBot emits on line 0, so the assertions skip the header
  // by passing `since=1` (or by checking the +1 offset on totalLines).
  it("returns all current buffer lines (including command header) when since=0", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.stdoutCb!(Buffer.from("a\nb\nc\n", "utf8"));
    const window = readLogWindow(handle, 0);
    expect(window.lines.length).toBe(4);
    expect(window.lines[0]).toMatch(/^\$ ssh /);
    expect(window.lines.slice(1)).toEqual(["a", "b", "c"]);
    expect(window.totalLines).toBe(4);
  });

  it("returns only the tail when since is mid-stream", () => {
    const { fn, last } = stubSpawnFactory();
    const handle = spawnRemoteBot(
      { host: host(), ttl: "5m", meetingURL: "u", participant: "p" },
      { spawn: fn as unknown as typeof import("node:child_process").spawn },
    );
    last.child!.stdoutCb!(Buffer.from("a\nb\nc\nd\n", "utf8"));
    // since=3 skips the header (line 0) + "a" (line 1) + "b" (line 2),
    // leaving "c" and "d".
    const window = readLogWindow(handle, 3);
    expect(window.lines).toEqual(["c", "d"]);
    expect(window.totalLines).toBe(5);
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
    // Header (1) + "a" + "b" = 3 total.
    expect(window.totalLines).toBe(3);
  });
});

describe("buildSshCommand", () => {
  function h(overrides: Partial<SshHost> = {}): SshHost {
    return {
      label: "h1",
      host: "my-host.lan",
      user: "alice",
      sshKey: null,
      reposPath: "/home/alice/videocall",
      notes: null,
      addedAt: 0,
      ...overrides,
    };
  }

  it("renders argv + display for a minimal spec", () => {
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
    });
    expect(r.argv[0]).toBe("ssh");
    expect(r.argv).toContain("-o");
    expect(r.argv).toContain("ConnectTimeout=10");
    expect(r.argv).toContain("alice@my-host.lan");
    // Last argv slot is the remote bash command.
    expect(r.argv[r.argv.length - 1]).toBe(r.remoteCommand);
    expect(r.remoteCommand).toContain("npm run bot");
    expect(r.display).toMatch(/^ssh /);
    expect(r.display).toContain("alice@my-host.lan");
    // The remote command lives inside a single-quoted argv slot in the
    // display rendering (it has spaces and shell metacharacters).
    expect(r.display).toContain("'cd '\\''/home/alice/videocall'\\''/e2e &&");
  });

  it("emits -i and -p when host has a key + non-default port", () => {
    const r = buildSshCommand(h({ host: "my-host.lan:2222", sshKey: "/keys/id_ed25519" }), {
      host: h({ host: "my-host.lan:2222", sshKey: "/keys/id_ed25519" }),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    expect(r.argv).toContain("-i");
    expect(r.argv).toContain("/keys/id_ed25519");
    expect(r.argv).toContain("-p");
    expect(r.argv).toContain("2222");
    expect(r.display).toContain("-i /keys/id_ed25519");
    expect(r.display).toContain("-p 2222");
  });

  it("omits -i when sshKey is null", () => {
    const r = buildSshCommand(h({ sshKey: null }), {
      host: h({ sshKey: null }),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    expect(r.argv).not.toContain("-i");
    expect(r.display).not.toMatch(/(^|\s)-i\s/);
  });

  it("propagates network override into the remote command", () => {
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
      network: "lossy_mobile",
    });
    expect(r.remoteCommand).toContain("--network 'lossy_mobile'");
  });

  it("emits --headless by default and omits it when explicitly false", () => {
    const headless = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    expect(headless.remoteCommand).toContain("--headless");
    const headed = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
      headless: false,
    });
    expect(headed.remoteCommand).not.toContain("--headless");
  });

  it("emits --auth when authBackend is set", () => {
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
      authBackend: "jwt",
    });
    expect(r.remoteCommand).toContain("--auth 'jwt'");
  });

  it("shell-escapes a display name with embedded special chars", () => {
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "alice",
      displayName: "Alice O'Reilly",
    });
    // The single quote in the display name is escaped via the standard
    // '\\'' dance, identical to what shellEscape produces.
    expect(r.remoteCommand).toContain("--display-name 'Alice O'\\''Reilly'");
  });

  it("display rendering is deterministic for the same inputs", () => {
    const a = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const b = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    expect(a.display).toBe(b.display);
    expect(a.argv).toEqual(b.argv);
  });
});
