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
    shellInit: null,
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
    // The last argv slot is the `bash -lc '<inner>'` wrapper. The
    // inner command (which contains `npm run bot`) lives inside the
    // single-quoted wrapper payload. We hard-code `bash` here so the
    // login-shell init chain reliably sources `~/.bash_profile` even
    // when the operator's default shell is zsh.
    const tail = args[args.length - 1] as string;
    expect(tail.startsWith("bash -lc ")).toBe(true);
    expect(tail).toContain("npm run bot");
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
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
    // The remote command is wrapped in `bash -lc '<inner>'` so the
    // operator's login PATH is loaded on the remote. The inner command
    // is also prefixed with an explicit `. ~/.bash_profile` source for
    // belt-and-suspenders coverage on hosts where bash's login-shell
    // init is intercepted.
    expect(firstLine).toContain("bash -lc");
    expect(firstLine).toContain(". ~/.bash_profile");
    expect(firstLine).toContain("alice");
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
      shellInit: null,
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
    // Last argv slot is the wrapped `bash -lc '<inner>'` form;
    // `remoteCommand` is the inner (unwrapped) `cd && npm` command,
    // exposed separately so the preview UI can display it on its own.
    // The default `. ~/.bash_profile` prefix shows up in the wrapper
    // payload but NOT in `remoteCommand` — `remoteCommand` stays just
    // the cd/npm chain so dashboards can show it cleanly.
    const tail = r.argv[r.argv.length - 1];
    expect(tail.startsWith("bash -lc ")).toBe(true);
    expect(tail).toContain(r.remoteCommand.replace(/'/g, "'\\''"));
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
    expect(r.remoteCommand).toContain("npm run bot");
    expect(r.remoteCommand.startsWith("cd '/home/alice/videocall'/e2e &&")).toBe(true);
    // `remoteCommand` is the inner cd/npm command only — the shell-init
    // prefix is part of the wrapper payload, not the cd/npm chain.
    expect(r.remoteCommand).not.toContain(".bash_profile");
    expect(r.display).toMatch(/^ssh /);
    expect(r.display).toContain("alice@my-host.lan");
    // The wrapper survives display rendering as a single-quoted argv
    // slot. We use the literal `bash` token rather than `$SHELL` so the
    // remote always runs a bash login shell regardless of the operator's
    // default shell.
    expect(r.display).toContain("'bash -lc");
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

  it("wraps the remote command in bash -lc so the login PATH loads", () => {
    // Regression test for `bash: npm: command not found` on remote
    // hosts whose node lives in a profile-installed PATH (nvm / fnm /
    // asdf / homebrew). SSH's default non-interactive non-login shell
    // does NOT source the operator's profile; wrapping the remote
    // command in `bash -lc` forces a bash login shell so the profile
    // is sourced and `npm` is on PATH. We hard-code `bash` (not
    // `$SHELL`) to avoid the zsh-default-shell pitfall on macOS, where
    // `zsh -lc` would source `~/.zprofile` but NOT `~/.bash_profile`.
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const tail = r.argv[r.argv.length - 1];
    // The wrapper is the literal first 9 chars of the argv slot.
    expect(tail.startsWith("bash -lc ")).toBe(true);
    // The wrapper payload contains the inner command, with each single
    // quote in the inner string escaped via the standard `'\''` dance
    // because the wrapper applies shellEscape to it once. The inner
    // command also includes the defensive `. ~/.bash_profile` prefix.
    expect(tail).toContain("npm run bot");
    expect(tail).toContain("cd '\\''/home/alice/videocall'\\''/e2e");
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
    // The wrapper is also visible in the display rendering — operators
    // copy-pasting the display string into a terminal reproduce the
    // exact spawn behaviour.
    expect(r.display).toContain("bash -lc");
  });

  it("the inner command is shell-escaped exactly once, not double-escaped", () => {
    // The remoteCommand field on the render output is the INNER
    // unwrapped cd/npm string. The wrapper concatenates the shell-init
    // prefix + remoteCommand and applies shellEscape exactly once, so
    // the result is recoverable by stripping the outer single-quote
    // pair (no `'\''\'\'''\''` triples at the wrapper boundary).
    const r = buildSshCommand(h(), {
      host: h(),
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
    });
    const tail = r.argv[r.argv.length - 1];
    // Build what a SINGLE shellEscape of (prefix + remoteCommand)
    // produces and check it appears verbatim in the tail.
    const prefix = "[ -f ~/.bash_profile ] && . ~/.bash_profile; ";
    const inner = prefix + r.remoteCommand;
    const singleEscaped = "'" + inner.replace(/'/g, "'\\''") + "'";
    expect(tail).toBe(`bash -lc ${singleEscaped}`);
    // Sanity: double-escaping would have produced the standard dance
    // applied twice. Assert that NEVER appears — even one occurrence
    // would mean we double-wrapped.
    const doubleEscaped = "'" + singleEscaped.replace(/'/g, "'\\''") + "'";
    expect(tail).not.toBe(`bash -lc ${doubleEscaped}`);
  });

  it("uses host.shellInit to override the default prefix when set", () => {
    // Operators whose node install lives in `~/.zshrc` (rather than
    // `~/.bash_profile`) can register a host with a `shellInit` field;
    // that value REPLACES the default prefix in the wrapper payload.
    const r = buildSshCommand(h({ shellInit: ". ~/.zshrc" }), {
      host: h({ shellInit: ". ~/.zshrc" }),
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const tail = r.argv[r.argv.length - 1];
    expect(tail).toContain(". ~/.zshrc;");
    // The default `. ~/.bash_profile` prefix MUST NOT also appear when
    // a custom shellInit replaces it.
    expect(tail).not.toContain(".bash_profile");
  });
});
