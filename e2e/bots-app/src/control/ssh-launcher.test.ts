import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

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
    shell: null,
    profileFile: null,
    preCommand: null,
    forwardSsoState: true,
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
  /** Bytes captured from `child.stdin.write(...)`. `null` when stdio
   *  is set to "ignore" (no stdin stream on the returned ChildProcess). */
  stdinBytes: Buffer | null;
  stdinEnded: boolean;
}

function stubSpawnFactory() {
  const last: { child: FakeChild | null } = { child: null };
  const fn = vi
    .fn()
    .mockImplementation((_cmd: string, _args: string[], options?: { stdio?: unknown[] }) => {
      const child: FakeChild = {
        stdoutCb: null,
        stderrCb: null,
        exitCb: null,
        errorCb: null,
        kill: vi.fn(),
        stdinBytes: null,
        stdinEnded: false,
      };
      last.child = child;
      // Only expose `stdin` on the returned ChildProcess when the launch
      // path actually opted into a pipe stream — mirrors the real
      // child_process semantics so the launcher's `child.stdin?.write(...)`
      // is a no-op when stdio[0] === "ignore".
      const stdinMode = options?.stdio?.[0];
      const stdin =
        stdinMode === "pipe"
          ? {
              write: (b: Buffer) => {
                child.stdinBytes =
                  child.stdinBytes === null ? Buffer.from(b) : Buffer.concat([child.stdinBytes, b]);
                return true;
              },
              end: () => {
                child.stdinEnded = true;
              },
            }
          : undefined;
      return {
        stdin,
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

/**
 * Create a temp SSO state file with arbitrary bytes (including a NUL
 * + high-byte sequence) so tests can assert binary round-tripping
 * through the stdin pipe — cookies frequently contain non-printable
 * characters that a UTF-8 string read would silently corrupt.
 */
function makeTempSsoState(payload: Buffer): { path: string; cleanup: () => void } {
  const dir = mkdtempSync(join(tmpdir(), "ssh-launcher-sso-"));
  const path = join(dir, "hcl-sso.json");
  writeFileSync(path, payload);
  return {
    path,
    cleanup: () => {
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        // best-effort
      }
    },
  };
}

describe("spawnRemoteBot", () => {
  it("invokes spawn('ssh', ...) with the host's argv", () => {
    const { fn } = stubSpawnFactory();
    spawnRemoteBot(
      {
        host: host({
          host: "example.com:2222",
          sshKey: "/keys/id",
          // Exercise the structured fields end-to-end so the wrapper
          // payload reflects `<profile source>; <preCommand>; <cd/npm>`.
          shell: "bash",
          profileFile: "~/.bash_profile",
        }),
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
    // The last argv slot is the `<shell> -lc '<inner>'` wrapper. The
    // inner command (which contains `npm run bot`) lives inside the
    // single-quoted wrapper payload. With profileFile=~/.bash_profile
    // the inner is prefixed with `[ -f ~/.bash_profile ] && . ~/.bash_profile; `.
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
        host: host({
          host: "example.com:2222",
          sshKey: "/keys/id",
          shell: "bash",
          profileFile: "~/.bash_profile",
        }),
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
    // The remote command is wrapped in `<shell> -lc '<inner>'` so the
    // operator's login PATH is loaded on the remote. With
    // profileFile=~/.bash_profile the inner is prefixed with an
    // explicit `. ~/.bash_profile` source line.
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

  // ──────────────────────────────────────────────────────────────────
  // SSO-state forwarding (stdin pipe + remote temp file)
  // ──────────────────────────────────────────────────────────────────

  it("wraps the remote command and pipes SSO state to stdin when forwardSsoState=true + local file exists + jwt auth", () => {
    const payload = Buffer.from("hcl-sso-cookies-payload\0\xffbinary", "binary");
    const tmp = makeTempSsoState(payload);
    try {
      const { fn, last } = stubSpawnFactory();
      spawnRemoteBot(
        {
          host: host({ forwardSsoState: true }),
          ttl: "5m",
          meetingURL: "https://example.com/meeting/X",
          participant: "alice",
          authBackend: "jwt",
          ssoStateFile: tmp.path,
        },
        { spawn: fn as unknown as typeof import("node:child_process").spawn },
      );
      expect(fn).toHaveBeenCalledTimes(1);
      const [, args, opts] = fn.mock.calls[0] as [string, string[], { stdio: unknown[] }];
      // The launcher MUST opt into a piped stdin so the SSO state can
      // actually be written; an "ignore" stdin would silently drop the
      // payload.
      expect(opts.stdio[0]).toBe("pipe");
      // The last argv slot carries the OUTER remote shell wrapper
      // with the mktemp + cat-to-temp + trap-EXIT prefix.
      const tail = args[args.length - 1] as string;
      expect(tail.startsWith('export T=$(mktemp); chmod 600 "$T"; cat > "$T"; trap')).toBe(true);
      expect(tail).toContain('trap "rm -f \\"$T\\"" EXIT');
      // The inner login-shell invocation should follow the outer
      // wrapper boilerplate and the npm command should reference the
      // exported `"$T"` token (double-quoted so the inner shell
      // expands it at runtime).
      expect(tail).toContain("bash -lc");
      expect(tail).toContain('--sso-state-file "$T"');
      // The stdin pipe must have received the exact bytes we wrote to
      // the temp file (including the NUL + high byte) and been closed
      // so the remote `cat > "$T"` sees EOF and unblocks.
      expect(last.child!.stdinBytes).not.toBeNull();
      expect(last.child!.stdinBytes!.equals(payload)).toBe(true);
      expect(last.child!.stdinEnded).toBe(true);
    } finally {
      tmp.cleanup();
    }
  });

  it("falls back to the unwrapped form and logs a warning when forwardSsoState=true + local file MISSING + jwt auth", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    try {
      const { fn, last } = stubSpawnFactory();
      const handle = spawnRemoteBot(
        {
          host: host({ forwardSsoState: true }),
          ttl: "5m",
          meetingURL: "u",
          participant: "alice",
          authBackend: "jwt",
          ssoStateFile: "/definitely/does/not/exist/hcl-sso.json",
          botId: "bot-XYZ",
        },
        { spawn: fn as unknown as typeof import("node:child_process").spawn },
      );
      const [, args, opts] = fn.mock.calls[0] as [string, string[], { stdio: unknown[] }];
      // No stdin pipe — we have nothing to feed.
      expect(opts.stdio[0]).toBe("ignore");
      const tail = args[args.length - 1] as string;
      // No outer mktemp/cat wrapper — falls back to the legacy
      // `<shell> -lc '...'` shape.
      expect(tail.startsWith("bash -lc ")).toBe(true);
      expect(tail).not.toContain("mktemp");
      expect(tail).not.toContain("--sso-state-file");
      // Warning must surface BOTH on console (for ops logs) AND in
      // recentLog (so the dashboard's per-bot log dialog shows it).
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn.mock.calls[0][0]).toContain("[bot-XYZ] ssh: no local SSO state file at");
      expect(handle.recentLog.join("\n")).toContain("no local SSO state file at");
      expect(last.child!.stdinBytes).toBeNull();
    } finally {
      warn.mockRestore();
    }
  });

  it("falls back to the unwrapped form when forwardSsoState=false regardless of local file presence", () => {
    const tmp = makeTempSsoState(Buffer.from("payload"));
    try {
      const { fn } = stubSpawnFactory();
      spawnRemoteBot(
        {
          host: host({ forwardSsoState: false }),
          ttl: "5m",
          meetingURL: "u",
          participant: "alice",
          authBackend: "jwt",
          ssoStateFile: tmp.path,
        },
        { spawn: fn as unknown as typeof import("node:child_process").spawn },
      );
      const [, args, opts] = fn.mock.calls[0] as [string, string[], { stdio: unknown[] }];
      expect(opts.stdio[0]).toBe("ignore");
      const tail = args[args.length - 1] as string;
      expect(tail.startsWith("bash -lc ")).toBe(true);
      expect(tail).not.toContain("mktemp");
      expect(tail).not.toContain("--sso-state-file");
    } finally {
      tmp.cleanup();
    }
  });

  it("falls back to the unwrapped form when authBackend !== 'jwt' even with forwardSsoState=true + local file", () => {
    // Storage-state and Guest auth do not consume SSO state — piping it
    // would just waste bandwidth + clutter the trace.
    const tmp = makeTempSsoState(Buffer.from("payload"));
    try {
      const { fn } = stubSpawnFactory();
      spawnRemoteBot(
        {
          host: host({ forwardSsoState: true }),
          ttl: "5m",
          meetingURL: "u",
          participant: "alice",
          authBackend: "storage-state",
          ssoStateFile: tmp.path,
        },
        { spawn: fn as unknown as typeof import("node:child_process").spawn },
      );
      const [, args, opts] = fn.mock.calls[0] as [string, string[], { stdio: unknown[] }];
      expect(opts.stdio[0]).toBe("ignore");
      const tail = args[args.length - 1] as string;
      expect(tail).not.toContain("mktemp");
      expect(tail).not.toContain("--sso-state-file");
    } finally {
      tmp.cleanup();
    }
  });

  it("preserves the legacy un-wrapped command shape byte-for-byte when forwardSsoState=false", () => {
    // Regression guard: operators with `forwardSsoState: false` must
    // get the EXACT same `bash -lc '<inner>'` shape as the launcher
    // emitted before this PR — no mktemp prefix, no stdin pipe, no
    // `--sso-state-file` flag, no trap EXIT.
    const tmp = makeTempSsoState(Buffer.from("payload"));
    try {
      const opted = stubSpawnFactory();
      const legacy = stubSpawnFactory();
      // Opt-out: forwardSsoState=false, jwt auth, file exists.
      spawnRemoteBot(
        {
          host: host({ forwardSsoState: false }),
          ttl: "5m",
          meetingURL: "u",
          participant: "alice",
          authBackend: "jwt",
          ssoStateFile: tmp.path,
        },
        { spawn: opted.fn as unknown as typeof import("node:child_process").spawn },
      );
      // Legacy: no ssoStateFile passed at all, plain authBackend.
      spawnRemoteBot(
        {
          host: host({ forwardSsoState: false }),
          ttl: "5m",
          meetingURL: "u",
          participant: "alice",
          authBackend: "jwt",
        },
        { spawn: legacy.fn as unknown as typeof import("node:child_process").spawn },
      );
      const optedArgs = opted.fn.mock.calls[0][1] as string[];
      const legacyArgs = legacy.fn.mock.calls[0][1] as string[];
      // Identical argv → identical wire-level SSH command.
      expect(optedArgs).toEqual(legacyArgs);
    } finally {
      tmp.cleanup();
    }
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
      shell: null,
      profileFile: null,
      preCommand: null,
      forwardSsoState: true,
      addedAt: 0,
      ...overrides,
    };
  }

  it("renders argv + display for a minimal spec", () => {
    // Pin profileFile so the wrapper payload includes the source line
    // we want to assert below. (With all three structured fields
    // unset the wrapper payload would be just the bare cd/npm chain.)
    const minimal = h({ profileFile: "~/.bash_profile" });
    const r = buildSshCommand(minimal, {
      host: minimal,
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
    // The structured-prefix source line shows up in the wrapper
    // payload but NOT in `remoteCommand` — `remoteCommand` stays just
    // the cd/npm chain so dashboards can show it cleanly.
    const tail = r.argv[r.argv.length - 1];
    expect(tail.startsWith("bash -lc ")).toBe(true);
    expect(tail).toContain(r.remoteCommand.replace(/'/g, "'\\''"));
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
    expect(r.remoteCommand).toContain("npm run bot");
    expect(r.remoteCommand.startsWith("cd '/home/alice/videocall'/e2e &&")).toBe(true);
    // `remoteCommand` is the inner cd/npm command only — the structured
    // prefix is part of the wrapper payload, not the cd/npm chain.
    expect(r.remoteCommand).not.toContain(".bash_profile");
    expect(r.display).toMatch(/^ssh /);
    expect(r.display).toContain("alice@my-host.lan");
    // The wrapper survives display rendering as a single-quoted argv
    // slot. The shell defaults to `bash` when host.shell is null.
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

  it("wraps the remote command in <shell> -lc so the login PATH loads", () => {
    // Regression test for `bash: npm: command not found` on remote
    // hosts whose node lives in a profile-installed PATH (nvm / fnm /
    // asdf / homebrew). SSH's default non-interactive non-login shell
    // does NOT source the operator's profile; wrapping the remote
    // command in `<shell> -lc` forces a login shell so the profile
    // is sourced and `npm` is on PATH. The shell defaults to `bash`
    // when host.shell is null (POSIX-defined login-shell init chain).
    const fixture = h({ profileFile: "~/.bash_profile" });
    const r = buildSshCommand(fixture, {
      host: fixture,
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const tail = r.argv[r.argv.length - 1];
    // The wrapper is the literal first 9 chars of the argv slot.
    expect(tail.startsWith("bash -lc ")).toBe(true);
    // The wrapper payload contains the inner command, with each single
    // quote in the inner string escaped via the standard `'\''` dance
    // because the wrapper applies shellEscape to it once. With
    // profileFile=~/.bash_profile the inner command also includes the
    // `. ~/.bash_profile` source line.
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
    // unwrapped cd/npm string. The wrapper concatenates the structured
    // prefix + remoteCommand and applies shellEscape exactly once, so
    // the result is recoverable by stripping the outer single-quote
    // pair (no `'\''\'\'''\''` triples at the wrapper boundary).
    const fixture = h({ profileFile: "~/.bash_profile" });
    const r = buildSshCommand(fixture, {
      host: fixture,
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

  it("uses host.shell to pick the wrapper shell and host.profileFile to source", () => {
    // Operators whose node install lives in `~/.zshrc` can register a
    // host with shell=zsh + profileFile=~/.zshrc; the wrapper becomes
    // `zsh -lc '[ -f ~/.zshrc ] && . ~/.zshrc; <cd/npm>'`.
    const zshHost = h({ shell: "zsh", profileFile: "~/.zshrc" });
    const r = buildSshCommand(zshHost, {
      host: zshHost,
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const tail = r.argv[r.argv.length - 1];
    expect(tail.startsWith("zsh -lc ")).toBe(true);
    expect(tail).toContain("[ -f ~/.zshrc ] && . ~/.zshrc;");
    // The default ~/.bash_profile path MUST NOT appear when the host
    // picks ~/.zshrc instead.
    expect(tail).not.toContain(".bash_profile");
  });

  it("includes host.preCommand after the profile source line", () => {
    // The structured-prefix order is profile-source THEN preCommand.
    // Each clause is terminated with `;` (not `&&`) so a non-zero exit
    // on either doesn't abort the npm chain.
    const nvmHost = h({
      profileFile: "~/.bash_profile",
      preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
    });
    const r = buildSshCommand(nvmHost, {
      host: nvmHost,
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
    });
    const tail = r.argv[r.argv.length - 1];
    // Profile source comes first, then preCommand, then cd/npm.
    expect(tail).toContain(
      "[ -f ~/.bash_profile ] && . ~/.bash_profile; . ~/.nvm/nvm.sh && nvm use 22;",
    );
  });
});
