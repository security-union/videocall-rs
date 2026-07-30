import { EventEmitter } from "node:events";
import { describe, expect, it, vi } from "vitest";

import type { SshHost } from "../control/ssh-hosts";
import { RemoteResourceManager, startRemoteSampler } from "./session";

/** Minimal ChildProcess stand-in for the injected spawn seam. */
class FakeChild extends EventEmitter {
  readonly stdin = { write: vi.fn(), end: vi.fn() };
  readonly stderr = new EventEmitter();
  readonly stdout = Object.assign(new EventEmitter(), { pipe: vi.fn() });
  readonly kill = vi.fn();
  pid = 4321;
}

function host(label: string, over: Partial<SshHost> = {}): SshHost {
  return {
    label,
    host: "box.intra",
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

describe("startRemoteSampler", () => {
  it("pipes the script over ssh bash -s and shell-escapes every arg", () => {
    const spawns: Array<{ cmd: string; args: string[] }> = [];
    const child = new FakeChild();
    const spawn = vi.fn((cmd: string, args: string[]) => {
      spawns.push({ cmd, args });
      return child as never;
    });

    const handle = startRemoteSampler("#!/usr/bin/env bash\necho hi\n", {
      host: host("lab-7"),
      intervalSec: 3,
      maxSeconds: 600,
      procGrep: "chrome",
      label: "run-x",
      spawn: spawn as never,
    });

    expect(spawns).toHaveLength(1);
    expect(spawns[0].cmd).toBe("ssh");
    // Connection flags come from the shared buildBaseSshArgs (BatchMode etc.).
    expect(spawns[0].args).toContain("BatchMode=yes");
    expect(spawns[0].args).toContain("alice@box.intra");
    // Keepalive so a mid-transfer stall cannot hang finalizeAll.
    expect(spawns[0].args).toContain("ServerAliveInterval=5");
    expect(spawns[0].args).toContain("ServerAliveCountMax=3");
    // The remote command is the last argv slot: bash -s -- <escaped args>.
    const remoteCmd = spawns[0].args[spawns[0].args.length - 1];
    expect(remoteCmd).toContain("bash -s --");
    expect(remoteCmd).toContain("--interval '3'");
    expect(remoteCmd).toContain("--max-seconds '600'");
    expect(remoteCmd).toContain("--proc-grep 'chrome'");
    // The script text is written to the remote stdin, then closed.
    expect(child.stdin.write).toHaveBeenCalledWith("#!/usr/bin/env bash\necho hi\n");
    expect(child.stdin.end).toHaveBeenCalled();
    expect(handle.remoteCsvPath).toContain("lab-7");
  });

  it("stop() kills the ssh session", () => {
    const child = new FakeChild();
    const spawn = vi.fn(() => child as never);
    const handle = startRemoteSampler("x", {
      host: host("h"),
      maxSeconds: 60,
      spawn: spawn as never,
    });
    handle.stop();
    expect(child.kill).toHaveBeenCalledWith("SIGTERM");
  });
});

describe("RemoteResourceManager", () => {
  it("starts exactly one sampler per distinct host label", async () => {
    let started = 0;
    const spawn = vi.fn(() => {
      started += 1;
      return new FakeChild() as never;
    });
    const mgr = new RemoteResourceManager({
      runDir: "/tmp/run",
      label: "run-x",
      maxSeconds: 60,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: spawn as never,
    });

    await mgr.ensureForHost(host("lab-7"));
    await mgr.ensureForHost(host("lab-7")); // same host → no second sampler
    await mgr.ensureForHost(host("lab-8"));

    expect(started).toBe(2);
  });

  it("does not start a sampler when the script text is empty", async () => {
    const spawn = vi.fn(() => new FakeChild() as never);
    const mgr = new RemoteResourceManager({
      runDir: "/tmp/run",
      label: "run-x",
      maxSeconds: 60,
      scriptText: "",
      spawn: spawn as never,
    });
    await mgr.ensureForHost(host("lab-7"));
    expect(spawn).not.toHaveBeenCalled();
  });
});
