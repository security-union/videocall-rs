import { mkdtempSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  addHost,
  buildRemoteLaunchCommand,
  buildSshArgsForLaunch,
  buildSshArgsForProbe,
  DEFAULT_SHELL_INIT_PREFIX,
  getHost,
  hostsFilePath,
  HOSTS_FILE_MODE,
  listHosts,
  removeHost,
  runSshProbe,
  shellEscape,
  SshHostExistsError,
  SshHostNotFoundError,
  SshHostValidationError,
  testHost,
  updateHost,
  validateHostField,
  validateLabelField,
  validateShellInitField,
  validateUserField,
  type SshHost,
} from "./ssh-hosts";

function tempDir(): string {
  return mkdtempSync(join(tmpdir(), "bots-ssh-hosts-"));
}

describe("ssh-hosts validation", () => {
  it("accepts alphanumeric + hyphen labels", () => {
    expect(validateLabelField("lab-mini-7")).toBe("lab-mini-7");
  });

  it("rejects labels with dots", () => {
    expect(() => validateLabelField("lab.mini")).toThrow(SshHostValidationError);
  });

  it("rejects labels starting with a hyphen", () => {
    expect(() => validateLabelField("-bad")).toThrow(SshHostValidationError);
  });

  it("rejects labels longer than 63 chars", () => {
    expect(() => validateLabelField("a".repeat(64))).toThrow(SshHostValidationError);
  });

  it("rejects hosts containing whitespace", () => {
    expect(() => validateHostField("bad host")).toThrow(SshHostValidationError);
  });

  it("rejects hosts containing shell metacharacters", () => {
    expect(() => validateHostField("a;b")).toThrow(SshHostValidationError);
    expect(() => validateHostField("a$b")).toThrow(SshHostValidationError);
    expect(() => validateHostField("`pwd`")).toThrow(SshHostValidationError);
  });

  it("accepts host with port suffix", () => {
    expect(validateHostField("example.com:2222")).toBe("example.com:2222");
  });

  it("rejects users with $ or `", () => {
    expect(() => validateUserField("a$b")).toThrow(SshHostValidationError);
    expect(() => validateUserField("`pwd`")).toThrow(SshHostValidationError);
  });

  it("accepts user='alice'", () => {
    expect(validateUserField("alice")).toBe("alice");
  });
});

describe("ssh-hosts persistence", () => {
  let dir: string;
  beforeEach(() => {
    dir = tempDir();
  });

  it("returns empty list when hosts.json is absent", async () => {
    expect(await listHosts(dir)).toEqual([]);
  });

  it("round-trips a saved host", async () => {
    const host: SshHost = await addHost(dir, {
      label: "lab-mini-7",
      host: "lab-mini-7.intra:2222",
      user: "alice",
      sshKey: null,
      reposPath: "/home/alice/videocall",
      notes: "lab Mac mini",
    });
    expect(host.label).toBe("lab-mini-7");
    expect(host.user).toBe("alice");
    expect(host.sshKey).toBeNull();
    const reloaded = await getHost(dir, "lab-mini-7");
    expect(reloaded?.host).toBe("lab-mini-7.intra:2222");
  });

  it("rejects a duplicate label with SshHostExistsError", async () => {
    await addHost(dir, {
      label: "dup",
      host: "h1",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    await expect(
      addHost(dir, {
        label: "dup",
        host: "h2",
        user: "bob",
        reposPath: "/home/bob/videocall",
      }),
    ).rejects.toThrow(SshHostExistsError);
  });

  it("rejects sshKey paths that don't exist on disk", async () => {
    await expect(
      addHost(dir, {
        label: "ghost",
        host: "h",
        user: "alice",
        sshKey: "/definitely/does/not/exist/id_rsa",
        reposPath: "/home/alice/videocall",
      }),
    ).rejects.toThrow(SshHostValidationError);
  });

  it("accepts an existing sshKey path", async () => {
    const keyPath = join(dir, "fake-key");
    writeFileSync(keyPath, "fake", "utf8");
    const host = await addHost(dir, {
      label: "with-key",
      host: "h",
      user: "alice",
      sshKey: keyPath,
      reposPath: "/home/alice/videocall",
    });
    expect(host.sshKey).toBe(keyPath);
  });

  it("writes hosts.json with mode 0o600", async () => {
    await addHost(dir, {
      label: "secret",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const st = statSync(hostsFilePath(dir));
    // Mask off setuid/setgid/sticky bits — only the bottom 9 perm bits
    // matter for our check.
    expect((st.mode & 0o777).toString(8)).toBe(HOSTS_FILE_MODE.toString(8));
  });

  it("updateHost patches fields and persists", async () => {
    await addHost(dir, {
      label: "patch-me",
      host: "old",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const patched = await updateHost(dir, "patch-me", {
      host: "new",
      notes: "moved",
    });
    expect(patched.host).toBe("new");
    expect(patched.notes).toBe("moved");
    const all = await listHosts(dir);
    expect(all[0].host).toBe("new");
  });

  it("updateHost throws SshHostNotFoundError when label is missing", async () => {
    await expect(updateHost(dir, "ghost", { host: "x" })).rejects.toThrow(SshHostNotFoundError);
  });

  it("removeHost deletes the row", async () => {
    await addHost(dir, {
      label: "gone",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    await removeHost(dir, "gone");
    expect(await getHost(dir, "gone")).toBeNull();
  });

  it("removeHost throws SshHostNotFoundError on missing", async () => {
    await expect(removeHost(dir, "nope")).rejects.toThrow(SshHostNotFoundError);
  });

  it("listHosts throws on malformed JSON (no silent data loss)", async () => {
    writeFileSync(hostsFilePath(dir), "{ not json", "utf8");
    await expect(listHosts(dir)).rejects.toThrow(SshHostValidationError);
  });
});

describe("shellEscape", () => {
  it("wraps a simple string in single quotes", () => {
    expect(shellEscape("alice")).toBe("'alice'");
  });

  it("handles an empty string", () => {
    expect(shellEscape("")).toBe("''");
  });

  it("escapes embedded single quotes via the standard '\\'' dance", () => {
    expect(shellEscape("a'b")).toBe("'a'\\''b'");
  });

  it("leaves spaces unmodified inside the quotes", () => {
    expect(shellEscape("a b c")).toBe("'a b c'");
  });

  it("does not interpret backticks or $ as special", () => {
    expect(shellEscape("`pwd`")).toBe("'`pwd`'");
    expect(shellEscape("$HOME")).toBe("'$HOME'");
  });
});

describe("buildRemoteLaunchCommand", () => {
  it("produces a single-line bash command with cd + npm run bot", () => {
    const cmd = buildRemoteLaunchCommand({
      reposPath: "/home/alice/videocall",
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
    });
    expect(cmd).toContain("cd '/home/alice/videocall'/e2e");
    expect(cmd).toContain("npm run bot -- run --headless");
    expect(cmd).toContain("--meeting-url 'https://example.com/meeting/X'");
    expect(cmd).toContain("--participant 'alice'");
    expect(cmd).toContain("--ttl '5m'");
  });

  it("omits --network when network is null or 'none'", () => {
    const cmd1 = buildRemoteLaunchCommand({
      reposPath: "/home/alice/videocall",
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      network: "none",
    });
    expect(cmd1).not.toContain("--network");
    const cmd2 = buildRemoteLaunchCommand({
      reposPath: "/home/alice/videocall",
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      network: null,
    });
    expect(cmd2).not.toContain("--network");
  });

  it("includes --network when set to a non-none profile", () => {
    const cmd = buildRemoteLaunchCommand({
      reposPath: "/home/alice/videocall",
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "alice",
      network: "lossy_mobile",
    });
    expect(cmd).toContain("--network 'lossy_mobile'");
  });

  it("escapes a participant with a single quote in it", () => {
    const cmd = buildRemoteLaunchCommand({
      reposPath: "/home/alice/videocall",
      ttl: "5m",
      meetingURL: "https://example.com/meeting/X",
      participant: "o'reilly",
    });
    expect(cmd).toContain("--participant 'o'\\''reilly'");
  });

  it("emits --headless by default and omits it when explicitly set false", () => {
    expect(
      buildRemoteLaunchCommand({
        reposPath: "/a",
        ttl: "5m",
        meetingURL: "u",
        participant: "p",
      }),
    ).toContain("--headless");
    const headed = buildRemoteLaunchCommand({
      reposPath: "/a",
      ttl: "5m",
      meetingURL: "u",
      participant: "p",
      headless: false,
    });
    expect(headed).not.toContain("--headless");
  });
});

describe("buildSshArgs*", () => {
  const host: SshHost = {
    label: "h1",
    host: "example.com:2222",
    user: "alice",
    sshKey: "/keys/id_ed25519",
    reposPath: "/home/alice/videocall",
    notes: null,
    shellInit: null,
    addedAt: 0,
  };

  it("buildSshArgsForProbe builds connect-timeout + key + port + user@host + probe cmd", () => {
    const args = buildSshArgsForProbe(host);
    expect(args).toContain("-o");
    expect(args).toContain("ConnectTimeout=5");
    expect(args).toContain("-i");
    expect(args).toContain("/keys/id_ed25519");
    expect(args).toContain("-p");
    expect(args).toContain("2222");
    expect(args).toContain("alice@example.com");
    expect(args[args.length - 1]).toContain("bots-app-probe-ok");
  });

  it("buildSshArgsForLaunch uses ConnectTimeout=10", () => {
    const args = buildSshArgsForLaunch(host, "echo hi");
    expect(args).toContain("ConnectTimeout=10");
    // The remote command is wrapped in `bash -lc <esc>` so the remote
    // shell runs as a bash login shell (which has a POSIX-defined init
    // chain that sources `~/.bash_profile`). Additionally, the inner
    // command is prefixed with `[ -f ~/.bash_profile ] && . ~/.bash_profile;`
    // as a defensive belt-and-suspenders measure — see the long comment
    // on `buildSshArgsForLaunch` for the rationale (macOS-zsh PATH
    // pitfall + visibility for operators reading the executed command).
    const tail = args[args.length - 1];
    expect(tail).toBe("bash -lc '[ -f ~/.bash_profile ] && . ~/.bash_profile; echo hi'");
  });

  it("buildSshArgsForLaunch wraps the remote command in bash -lc (login shell)", () => {
    // Regression: SSH's default non-interactive non-login shell does
    // NOT source ~/.bash_profile / ~/.profile / ~/.zprofile, so
    // operators who installed node via nvm / fnm / asdf / homebrew used
    // to hit `bash: npm: command not found`. We hard-code `bash` (not
    // `$SHELL`) because `bash -l` has a POSIX-defined login-shell init
    // chain that always reads `~/.bash_profile` — `$SHELL` would expand
    // to `/bin/zsh` on macOS, and `zsh -lc` does not source it.
    const args = buildSshArgsForLaunch(host, "cd '/p'/e2e && npm run bot");
    const tail = args[args.length - 1];
    expect(tail.startsWith("bash -lc ")).toBe(true);
    // Inner command is single-quoted with `'\''` for embedded quotes —
    // exactly one layer of escaping (not double). The defensive
    // `[ -f ~/.bash_profile ] && . ~/.bash_profile;` prefix runs first.
    expect(tail).toBe(
      "bash -lc '[ -f ~/.bash_profile ] && . ~/.bash_profile; cd '\\''/p'\\''/e2e && npm run bot'",
    );
  });

  it("prefixes the inner command with `. ~/.bash_profile` by default", () => {
    // Belt-and-suspenders: even when bash -l would auto-source it, we
    // explicitly source it again. The `[ -f … ] &&` guard makes the
    // prefix safe on hosts that lack ~/.bash_profile.
    const args = buildSshArgsForLaunch(host, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
    // Trailing `;` (not `&&`) so a non-zero exit from the source
    // doesn't abort the npm chain.
    expect(tail).not.toContain(".bash_profile &&");
    // Sanity: the default-prefix constant matches what's emitted.
    expect(tail).toContain(DEFAULT_SHELL_INIT_PREFIX);
  });

  it("uses operator-supplied shellInit when set, replacing the default", () => {
    // When the host has a non-empty `shellInit`, that snippet REPLACES
    // the default `. ~/.bash_profile` prefix (no concatenation). This
    // covers nvm-only-in-zshrc operators and similar non-standard
    // setups.
    const zshHost: SshHost = { ...host, shellInit: ". ~/.zshrc" };
    const args = buildSshArgsForLaunch(zshHost, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toBe("bash -lc '. ~/.zshrc; npm run bot'");
    // The default prefix MUST NOT also appear — `shellInit` is a full
    // replacement, not an addition.
    expect(tail).not.toContain(".bash_profile");
  });

  it("falls back to the default prefix when shellInit is an empty string", () => {
    // Defensive: an empty string and `null` both mean "use default".
    // (Persistence canonicalizes empty → null at write time, but the
    // builder must tolerate either at read time.)
    const emptyHost: SshHost = { ...host, shellInit: "" };
    const args = buildSshArgsForLaunch(emptyHost, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
  });

  it("strips trailing terminators from shellInit so the prefix joins cleanly", () => {
    // Operators may end their snippet with `;` or `&&` or trailing
    // whitespace. The builder canonicalizes the trailing token so the
    // emitted prefix always reads `<snippet>; ` regardless of input.
    const trailHost: SshHost = { ...host, shellInit: ". ~/.zshrc &&" };
    const args = buildSshArgsForLaunch(trailHost, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toBe("bash -lc '. ~/.zshrc; npm run bot'");
  });

  it("omits -i when sshKey is null", () => {
    const args = buildSshArgsForProbe({ ...host, sshKey: null });
    expect(args).not.toContain("-i");
  });

  it("omits -p when host has no :port suffix", () => {
    const args = buildSshArgsForProbe({ ...host, host: "example.com" });
    expect(args).not.toContain("-p");
    expect(args).toContain("alice@example.com");
  });
});

describe("validateShellInitField", () => {
  it("accepts a typical zshrc source line", () => {
    expect(validateShellInitField(". ~/.zshrc")).toBe(". ~/.zshrc");
  });

  it("accepts a chained nvm + npm setup", () => {
    const v = ". ~/.nvm/nvm.sh && nvm use 22";
    expect(validateShellInitField(v)).toBe(v);
  });

  it("rejects an empty string (callers must pass null instead)", () => {
    expect(() => validateShellInitField("")).toThrow(SshHostValidationError);
  });

  it("rejects embedded newlines (would break single-line bash contract)", () => {
    expect(() => validateShellInitField(". ~/.zshrc\nrm -rf /")).toThrow(SshHostValidationError);
    expect(() => validateShellInitField("a\rb")).toThrow(SshHostValidationError);
  });

  it("rejects embedded NUL bytes", () => {
    expect(() => validateShellInitField("a\0b")).toThrow(SshHostValidationError);
  });

  it("rejects strings longer than 512 chars", () => {
    expect(() => validateShellInitField("a".repeat(513))).toThrow(SshHostValidationError);
    // 512 exactly is the boundary — must be accepted.
    const exact = "a".repeat(512);
    expect(validateShellInitField(exact)).toBe(exact);
  });
});

describe("ssh-hosts shellInit persistence", () => {
  let dir: string;
  beforeEach(() => {
    dir = tempDir();
  });

  it("addHost persists shellInit when supplied", async () => {
    const host = await addHost(dir, {
      label: "zsh-mac",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shellInit: ". ~/.zshrc",
    });
    expect(host.shellInit).toBe(". ~/.zshrc");
    const reloaded = await getHost(dir, "zsh-mac");
    expect(reloaded?.shellInit).toBe(". ~/.zshrc");
  });

  it("addHost defaults shellInit to null when not supplied", async () => {
    const host = await addHost(dir, {
      label: "default-init",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    expect(host.shellInit).toBeNull();
  });

  it("addHost treats empty-string shellInit as null", async () => {
    const host = await addHost(dir, {
      label: "empty-init",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shellInit: "",
    });
    expect(host.shellInit).toBeNull();
  });

  it("updateHost sets shellInit when patched with a value", async () => {
    await addHost(dir, {
      label: "init-me",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const patched = await updateHost(dir, "init-me", { shellInit: ". ~/.zshrc" });
    expect(patched.shellInit).toBe(". ~/.zshrc");
  });

  it("updateHost clears shellInit when patched with null", async () => {
    await addHost(dir, {
      label: "clear-me",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shellInit: ". ~/.zshrc",
    });
    const patched = await updateHost(dir, "clear-me", { shellInit: null });
    expect(patched.shellInit).toBeNull();
  });

  it("listHosts tolerates legacy rows that lack the shellInit key", async () => {
    // Forward-compat: registries written before this field existed
    // simply lack the key. They must load as `shellInit: null` rather
    // than failing validation.
    writeFileSync(
      hostsFilePath(dir),
      JSON.stringify({
        version: 1,
        hosts: [
          {
            label: "legacy",
            host: "h",
            user: "alice",
            sshKey: null,
            reposPath: "/home/alice/videocall",
            notes: null,
            addedAt: 0,
          },
        ],
      }),
      "utf8",
    );
    const all = await listHosts(dir);
    expect(all).toHaveLength(1);
    expect(all[0].shellInit).toBeNull();
  });
});

describe("testHost (with stubbed spawn)", () => {
  let dir: string;
  beforeEach(() => {
    dir = tempDir();
  });

  function stubSpawn(opts: { stdout?: string; stderr?: string; code: number | null }) {
    return vi.fn().mockImplementation(() => {
      const stdoutHandlers: Array<(b: Buffer) => void> = [];
      const stderrHandlers: Array<(b: Buffer) => void> = [];
      const exitHandlers: Array<(code: number | null) => void> = [];
      const errHandlers: Array<(err: Error) => void> = [];
      const child = {
        stdout: {
          on: (event: string, cb: (b: Buffer) => void) => {
            if (event === "data") stdoutHandlers.push(cb);
          },
        },
        stderr: {
          on: (event: string, cb: (b: Buffer) => void) => {
            if (event === "data") stderrHandlers.push(cb);
          },
        },
        on: (event: string, cb: (...args: unknown[]) => void) => {
          if (event === "exit") exitHandlers.push(cb as (code: number | null) => void);
          if (event === "error") errHandlers.push(cb as (err: Error) => void);
        },
      };
      // Fire stdout/stderr + exit on the next microtask so the
      // listeners have been registered.
      queueMicrotask(() => {
        if (opts.stdout) {
          for (const h of stdoutHandlers) h(Buffer.from(opts.stdout, "utf8"));
        }
        if (opts.stderr) {
          for (const h of stderrHandlers) h(Buffer.from(opts.stderr, "utf8"));
        }
        for (const h of exitHandlers) h(opts.code);
      });
      return child;
    });
  }

  it("returns ok=true when the probe sentinel shows up in stdout and exit=0", async () => {
    await addHost(dir, {
      label: "good",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const fakeSpawn = stubSpawn({
      stdout: "bots-app-probe-ok\nLinux foo 5.10\n",
      code: 0,
    });
    const result = await testHost(dir, "good", {
      spawn: fakeSpawn as unknown as typeof import("node:child_process").spawn,
    });
    expect(result.ok).toBe(true);
    expect(result.output).toContain("bots-app-probe-ok");
  });

  it("returns ok=false with stderr text when the probe fails", async () => {
    await addHost(dir, {
      label: "bad",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const fakeSpawn = stubSpawn({
      stderr: "Permission denied (publickey).\n",
      code: 255,
    });
    const result = await testHost(dir, "bad", {
      spawn: fakeSpawn as unknown as typeof import("node:child_process").spawn,
    });
    expect(result.ok).toBe(false);
    expect(result.error).toContain("Permission denied");
  });

  it("returns ok=false when ssh exits 0 but stdout lacks the probe sentinel", async () => {
    // Defense in depth: a misbehaving network middlebox could
    // intercept and return 200 without ever reaching the host.
    const host: SshHost = {
      label: "weird",
      host: "h",
      user: "alice",
      sshKey: null,
      reposPath: "/home/alice/videocall",
      notes: null,
      shellInit: null,
      addedAt: 0,
    };
    const fakeSpawn = stubSpawn({ stdout: "different output\n", code: 0 });
    const result = await runSshProbe(host, {
      spawn: fakeSpawn as unknown as typeof import("node:child_process").spawn,
    });
    expect(result.ok).toBe(false);
  });
});
