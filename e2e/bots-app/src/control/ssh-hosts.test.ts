import { mkdtempSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  addHost,
  buildRemoteCommandPrefix,
  buildRemoteLaunchCommand,
  buildSshArgsForLaunch,
  buildSshArgsForProbe,
  defaultProfileFileForShell,
  DEFAULT_SHELL,
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
  validatePreCommandField,
  validateProfileFileField,
  validateShellField,
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
    shell: null,
    profileFile: null,
    preCommand: null,
    forwardSsoState: true,
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
    // Default host has shell=null, profileFile=null, preCommand=null.
    // Wrapper becomes `bash -lc '<inner>'` with NO prefix.
    const tail = args[args.length - 1];
    expect(tail).toBe("bash -lc 'echo hi'");
  });

  it("emits a profileFile source line when host.profileFile is set", () => {
    const withProfile: SshHost = { ...host, profileFile: "~/.bash_profile" };
    const args = buildSshArgsForLaunch(withProfile, "cd '/p'/e2e && npm run bot");
    const tail = args[args.length - 1];
    expect(tail.startsWith("bash -lc ")).toBe(true);
    // Source line is wrapped in `[ -f … ] &&` and terminated with `; `
    // so the rest of the chain runs even if the file is missing.
    expect(tail).toBe(
      "bash -lc '[ -f ~/.bash_profile ] && . ~/.bash_profile; cd '\\''/p'\\''/e2e && npm run bot'",
    );
  });

  it("uses host.shell to pick the wrapper shell (zsh)", () => {
    // Per-host shell choice — `<shell> -lc …` rather than the
    // hard-coded `bash -lc …`.
    const zshHost: SshHost = { ...host, shell: "zsh", profileFile: "~/.zshrc" };
    const args = buildSshArgsForLaunch(zshHost, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toBe("zsh -lc '[ -f ~/.zshrc ] && . ~/.zshrc; npm run bot'");
  });

  it("defaults the wrapper shell to bash when host.shell is null", () => {
    const args = buildSshArgsForLaunch(host, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail.startsWith("bash -lc ")).toBe(true);
  });

  it("appends host.preCommand after the profile source line", () => {
    // preCommand runs AFTER sourcing the profile, BEFORE the cd/npm
    // chain. Both terminated with `;` so a non-zero exit on either
    // doesn't kill the launch.
    const nvmHost: SshHost = {
      ...host,
      profileFile: "~/.bash_profile",
      preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
    };
    const args = buildSshArgsForLaunch(nvmHost, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toBe(
      "bash -lc '[ -f ~/.bash_profile ] && . ~/.bash_profile; . ~/.nvm/nvm.sh && nvm use 22; npm run bot'",
    );
  });

  it("emits only the preCommand when profileFile is null but preCommand is set", () => {
    const preOnly: SshHost = {
      ...host,
      profileFile: null,
      preCommand: "export PATH=$HOME/.local/bin:$PATH",
    };
    const args = buildSshArgsForLaunch(preOnly, "npm run bot");
    const tail = args[args.length - 1];
    expect(tail).toBe("bash -lc 'export PATH=$HOME/.local/bin:$PATH; npm run bot'");
  });

  it("emits no prefix when shell+profileFile+preCommand are all unset", () => {
    const args = buildSshArgsForLaunch(host, "npm run bot");
    const tail = args[args.length - 1];
    // No `[ -f … ]` source line, no preCommand prefix — just `bash -lc 'npm run bot'`.
    expect(tail).toBe("bash -lc 'npm run bot'");
  });

  it("strips trailing terminators from preCommand so the prefix joins cleanly", () => {
    // Operators may end their snippet with `;` or `&&` or trailing
    // whitespace. The builder canonicalizes the trailing token so the
    // emitted prefix always reads `<snippet>; ` regardless of input.
    const trailHost: SshHost = { ...host, preCommand: ". ~/.zshrc &&" };
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

describe("buildRemoteCommandPrefix", () => {
  function h(over: Partial<SshHost> = {}): SshHost {
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
      ...over,
    };
  }

  it("returns the empty string when both profileFile and preCommand are null", () => {
    expect(buildRemoteCommandPrefix(h())).toBe("");
  });

  it("emits only the profile source line when preCommand is null", () => {
    expect(buildRemoteCommandPrefix(h({ profileFile: "~/.bash_profile" }))).toBe(
      "[ -f ~/.bash_profile ] && . ~/.bash_profile; ",
    );
  });

  it("emits only the preCommand when profileFile is null", () => {
    expect(buildRemoteCommandPrefix(h({ preCommand: "nvm use 22" }))).toBe("nvm use 22; ");
  });

  it("emits both clauses in order: profile source first, preCommand second", () => {
    const prefix = buildRemoteCommandPrefix(
      h({
        profileFile: "~/.zshrc",
        preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
      }),
    );
    expect(prefix).toBe("[ -f ~/.zshrc ] && . ~/.zshrc; . ~/.nvm/nvm.sh && nvm use 22; ");
  });
});

describe("defaultProfileFileForShell", () => {
  it("returns ~/.bash_profile for bash", () => {
    expect(defaultProfileFileForShell("bash")).toBe("~/.bash_profile");
  });

  it("returns ~/.zshrc for zsh", () => {
    expect(defaultProfileFileForShell("zsh")).toBe("~/.zshrc");
  });

  it("returns null for sh (POSIX, no convention)", () => {
    expect(defaultProfileFileForShell("sh")).toBeNull();
  });

  it("returns null for custom absolute shell paths", () => {
    expect(defaultProfileFileForShell("/opt/homebrew/bin/zsh")).toBeNull();
  });

  it("defaults to ~/.bash_profile when shell is null (matches DEFAULT_SHELL=bash)", () => {
    expect(defaultProfileFileForShell(null)).toBe("~/.bash_profile");
    expect(DEFAULT_SHELL).toBe("bash");
  });
});

describe("validateShellField", () => {
  it("accepts bare shell names", () => {
    expect(validateShellField("bash")).toBe("bash");
    expect(validateShellField("zsh")).toBe("zsh");
    expect(validateShellField("sh")).toBe("sh");
    expect(validateShellField("fish")).toBe("fish");
  });

  it("accepts absolute shell paths", () => {
    expect(validateShellField("/opt/homebrew/bin/zsh")).toBe("/opt/homebrew/bin/zsh");
    expect(validateShellField("/bin/bash")).toBe("/bin/bash");
  });

  it("rejects shell metacharacters (defense-in-depth on the wrapper)", () => {
    expect(() => validateShellField("bash;rm -rf /")).toThrow(SshHostValidationError);
    expect(() => validateShellField("bash$IFS")).toThrow(SshHostValidationError);
    expect(() => validateShellField("`pwd`")).toThrow(SshHostValidationError);
    expect(() => validateShellField("bash space")).toThrow(SshHostValidationError);
  });

  it("rejects an empty string (callers pass null instead)", () => {
    expect(() => validateShellField("")).toThrow(SshHostValidationError);
  });
});

describe("validateProfileFileField", () => {
  it("accepts ~ prefixed paths", () => {
    expect(validateProfileFileField("~/.bash_profile")).toBe("~/.bash_profile");
    expect(validateProfileFileField("~/.zshrc")).toBe("~/.zshrc");
  });

  it("accepts absolute paths", () => {
    expect(validateProfileFileField("/etc/profile")).toBe("/etc/profile");
  });

  it("rejects whitespace and shell metacharacters", () => {
    expect(() => validateProfileFileField("~/has space")).toThrow(SshHostValidationError);
    expect(() => validateProfileFileField("~/$evil")).toThrow(SshHostValidationError);
    expect(() => validateProfileFileField("~/;rm")).toThrow(SshHostValidationError);
  });
});

describe("validatePreCommandField", () => {
  it("accepts a typical nvm chain", () => {
    const v = ". ~/.nvm/nvm.sh && nvm use 22";
    expect(validatePreCommandField(v)).toBe(v);
  });

  it("accepts a PATH export", () => {
    const v = "export PATH=$HOME/.local/bin:$PATH";
    expect(validatePreCommandField(v)).toBe(v);
  });

  it("rejects an empty string (callers pass null instead)", () => {
    expect(() => validatePreCommandField("")).toThrow(SshHostValidationError);
  });

  it("rejects embedded newlines (would break single-line bash contract)", () => {
    expect(() => validatePreCommandField(". ~/.zshrc\nrm -rf /")).toThrow(SshHostValidationError);
    expect(() => validatePreCommandField("a\rb")).toThrow(SshHostValidationError);
  });

  it("rejects embedded NUL bytes", () => {
    expect(() => validatePreCommandField("a\0b")).toThrow(SshHostValidationError);
  });

  it("rejects strings longer than 512 chars", () => {
    expect(() => validatePreCommandField("a".repeat(513))).toThrow(SshHostValidationError);
    // 512 exactly is the boundary — must be accepted.
    const exact = "a".repeat(512);
    expect(validatePreCommandField(exact)).toBe(exact);
  });
});

describe("forwardSsoState persistence + forward-compat", () => {
  let dir: string;
  beforeEach(() => {
    dir = tempDir();
  });

  it("defaults forwardSsoState to true on a newly-created host when omitted", async () => {
    const h = await addHost(dir, {
      label: "default-sso",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    expect(h.forwardSsoState).toBe(true);
    const reloaded = await getHost(dir, "default-sso");
    expect(reloaded?.forwardSsoState).toBe(true);
  });

  it("round-trips forwardSsoState=false explicitly through addHost + listHosts", async () => {
    const h = await addHost(dir, {
      label: "opt-out",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      forwardSsoState: false,
    });
    expect(h.forwardSsoState).toBe(false);
    const reloaded = await getHost(dir, "opt-out");
    expect(reloaded?.forwardSsoState).toBe(false);
  });

  it("updateHost can flip forwardSsoState OFF and back ON", async () => {
    await addHost(dir, {
      label: "flippy",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    let patched = await updateHost(dir, "flippy", { forwardSsoState: false });
    expect(patched.forwardSsoState).toBe(false);
    patched = await updateHost(dir, "flippy", { forwardSsoState: true });
    expect(patched.forwardSsoState).toBe(true);
  });

  it("loads legacy host JSON without forwardSsoState as forwardSsoState=true", async () => {
    // Forward-compat: registries persisted before this field existed
    // simply lack the key. They MUST load with `forwardSsoState: true`
    // — the safer default for most operators, who are running bots
    // against HCL-SSO-gated meetings.
    writeFileSync(
      hostsFilePath(dir),
      JSON.stringify({
        version: 1,
        hosts: [
          {
            label: "legacy-no-field",
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
    expect(all[0].forwardSsoState).toBe(true);
  });

  it("rejects a stored row whose forwardSsoState is a non-boolean", async () => {
    writeFileSync(
      hostsFilePath(dir),
      JSON.stringify({
        version: 1,
        hosts: [
          {
            label: "bad-type",
            host: "h",
            user: "alice",
            sshKey: null,
            reposPath: "/home/alice/videocall",
            notes: null,
            forwardSsoState: "yes",
            addedAt: 0,
          },
        ],
      }),
      "utf8",
    );
    await expect(listHosts(dir)).rejects.toThrow(SshHostValidationError);
  });
});

describe("ssh-hosts shell/profileFile/preCommand persistence", () => {
  let dir: string;
  beforeEach(() => {
    dir = tempDir();
  });

  it("addHost persists all three structured fields when supplied", async () => {
    const host = await addHost(dir, {
      label: "zsh-mac",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shell: "zsh",
      profileFile: "~/.zshrc",
      preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
    });
    expect(host.shell).toBe("zsh");
    expect(host.profileFile).toBe("~/.zshrc");
    expect(host.preCommand).toBe(". ~/.nvm/nvm.sh && nvm use 22");
    const reloaded = await getHost(dir, "zsh-mac");
    expect(reloaded?.shell).toBe("zsh");
    expect(reloaded?.profileFile).toBe("~/.zshrc");
    expect(reloaded?.preCommand).toBe(". ~/.nvm/nvm.sh && nvm use 22");
  });

  it("addHost defaults each structured field to null when not supplied", async () => {
    const host = await addHost(dir, {
      label: "default-init",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    expect(host.shell).toBeNull();
    expect(host.profileFile).toBeNull();
    expect(host.preCommand).toBeNull();
  });

  it("addHost treats empty-string values for the structured fields as null", async () => {
    const host = await addHost(dir, {
      label: "empty-init",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shell: "",
      profileFile: "",
      preCommand: "",
    });
    expect(host.shell).toBeNull();
    expect(host.profileFile).toBeNull();
    expect(host.preCommand).toBeNull();
  });

  it("updateHost sets each field when patched with a value", async () => {
    await addHost(dir, {
      label: "init-me",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const patched = await updateHost(dir, "init-me", {
      shell: "zsh",
      profileFile: "~/.zshrc",
      preCommand: ". ~/.nvm/nvm.sh",
    });
    expect(patched.shell).toBe("zsh");
    expect(patched.profileFile).toBe("~/.zshrc");
    expect(patched.preCommand).toBe(". ~/.nvm/nvm.sh");
  });

  it("updateHost clears each field when patched with null", async () => {
    await addHost(dir, {
      label: "clear-me",
      host: "h",
      user: "alice",
      reposPath: "/home/alice/videocall",
      shell: "zsh",
      profileFile: "~/.zshrc",
      preCommand: ". ~/.nvm/nvm.sh",
    });
    const patched = await updateHost(dir, "clear-me", {
      shell: null,
      profileFile: null,
      preCommand: null,
    });
    expect(patched.shell).toBeNull();
    expect(patched.profileFile).toBeNull();
    expect(patched.preCommand).toBeNull();
  });

  it("listHosts tolerates legacy rows that lack the structured fields", async () => {
    // Forward-compat: registries written before these fields existed
    // (or registries that may have used the prior `shellInit` field
    // from earlier in this PR) simply lack the keys. They must load
    // with all three set to `null` rather than failing validation.
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
            // Legacy `shellInit` field from a previous iteration of this PR
            // — must be silently dropped (no error, no migration).
            shellInit: ". ~/.zshrc",
            addedAt: 0,
          },
        ],
      }),
      "utf8",
    );
    const all = await listHosts(dir);
    expect(all).toHaveLength(1);
    expect(all[0].shell).toBeNull();
    expect(all[0].profileFile).toBeNull();
    expect(all[0].preCommand).toBeNull();
    // Sanity: the legacy `shellInit` key did NOT leak into the
    // returned object as some untyped property.
    expect((all[0] as unknown as Record<string, unknown>).shellInit).toBeUndefined();
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
      shell: null,
      profileFile: null,
      preCommand: null,
      forwardSsoState: true,
      addedAt: 0,
    };
    const fakeSpawn = stubSpawn({ stdout: "different output\n", code: 0 });
    const result = await runSshProbe(host, {
      spawn: fakeSpawn as unknown as typeof import("node:child_process").spawn,
    });
    expect(result.ok).toBe(false);
  });
});
