import { EventEmitter } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { SshHost } from "../control/ssh-hosts";
import { RemoteResourceManager, startRemoteSampler } from "./session";

const SOURCE = fileURLToPath(new URL("./session.ts", import.meta.url));

/** A complete, correctly-prefixed resource line an operator value must not be able to start. */
const FORGED = "[resource] FORGED-BY-OPERATOR-VALUE";

class FakeChild extends EventEmitter {
  readonly stdin = { write: vi.fn(), end: vi.fn() };
  readonly stderr = new EventEmitter();
  readonly stdout = Object.assign(new EventEmitter(), { pipe: vi.fn() });
  readonly kill = vi.fn();
  pid = 4321;
}

const dirs: string[] = [];

afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

function host(label: string): SshHost {
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
  };
}

/** Capture every console line a body writes. */
async function captured(body: () => void | Promise<void>): Promise<string[]> {
  const written: string[] = [];
  const record = (...a: unknown[]): void => {
    written.push(a.map(String).join(" "));
  };
  const spies = [
    vi.spyOn(console, "warn").mockImplementation(record),
    vi.spyOn(console, "log").mockImplementation(record),
    vi.spyOn(console, "error").mockImplementation(record),
  ];
  try {
    await body();
  } finally {
    for (const spy of spies) spy.mockRestore();
  }
  return written;
}

/**
 * Both halves are required: no emitted physical line may START with the forged
 * marker, AND the marker must still be present somewhere — otherwise a probe
 * that produced no output at all would pass.
 */
function expectCollapsed(written: string[]): void {
  const lines = written.join("\n").split(/[\r\n]/);
  expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
  expect(lines.filter((l) => l.includes(FORGED)).length).toBeGreaterThan(0);
}

describe("[resource] log-line forgery (#2480)", () => {
  it("collapses remote sampler stderr, the least trusted input on this path", async () => {
    const child = new FakeChild();
    const written = await captured(() => {
      startRemoteSampler("#!/usr/bin/env bash\n", {
        host: host("lab-7"),
        maxSeconds: 60,
        spawn: (() => child) as never,
      });
      child.stderr.emit("data", Buffer.from(`sampler died\n${FORGED}`, "utf8"));
    });
    expectCollapsed(written);
  });

  it("keeps a forged host label out of the remote-sampler stderr line", async () => {
    const child = new FakeChild();
    const written = await captured(() => {
      startRemoteSampler("#!/usr/bin/env bash\n", {
        host: host(`lab-7\n${FORGED}`),
        maxSeconds: 60,
        spawn: (() => child) as never,
      });
      child.stderr.emit("data", Buffer.from("sampler died", "utf8"));
    });
    expectCollapsed(written);
  });

  it("keeps a forged host label out of the sampler-started line", async () => {
    const mgr = new RemoteResourceManager({
      runDir: "/tmp/run",
      label: "run-x",
      maxSeconds: 60,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: (() => new FakeChild()) as never,
    });
    expectCollapsed(await captured(() => mgr.ensureForHost(host(`lab-7\n${FORGED}`))));
  });

  it("keeps a forged host label out of the sampler-start-failure line", async () => {
    const mgr = new RemoteResourceManager({
      runDir: "/tmp/run",
      label: "run-x",
      maxSeconds: 60,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: (() => {
        throw new Error("spawn refused");
      }) as never,
    });
    expectCollapsed(await captured(() => mgr.ensureForHost(host(`lab-7\n${FORGED}`))));
  });

  it("keeps a forged host label and remote stderr out of the retrieve-exit line", async () => {
    const dir = mkdtempSync(join(tmpdir(), "res-logline-"));
    dirs.push(dir);
    const child = new FakeChild();
    const handle = startRemoteSampler("#!/usr/bin/env bash\n", {
      host: host(`lab-7\n${FORGED}`),
      maxSeconds: 60,
      retrieveStallMs: 60_000,
      spawn: (() => child) as never,
    });
    const written = await captured(async () => {
      const pending = handle.retrieve(join(dir, "raw.csv"));
      child.stderr.emit("data", Buffer.from(`cat: no such file\n${FORGED}`, "utf8"));
      child.emit("close", 1);
      expect(await pending).toBe(0);
    });
    expectCollapsed(written);
  });

  it("keeps a forged host label out of the retrieve-spawn-failure and no-CSV lines", async () => {
    const dir = mkdtempSync(join(tmpdir(), "res-logline-"));
    dirs.push(dir);
    // Call 1 starts the sampler; call 2 is `finalizeAll`'s `cat` retrieve.
    let spawns = 0;
    const mgr = new RemoteResourceManager({
      runDir: dir,
      label: "run-x",
      maxSeconds: 60,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: (() => {
        spawns += 1;
        if (spawns > 1) throw new Error(`ssh missing\n${FORGED}`);
        return new FakeChild();
      }) as never,
    });
    const written = await captured(async () => {
      await mgr.ensureForHost(host(`lab-7\n${FORGED}`));
      expect(await mgr.finalizeAll()).toEqual([]);
    });
    expect(spawns).toBe(2);
    expectCollapsed(written);
  });
});

describe("session.ts marker routing (#2480)", () => {
  /** Source with comments removed, so a doc comment naming the marker is not a hit. */
  function code(): string {
    return readFileSync(SOURCE, "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .split("\n")
      .filter((l) => !l.trimStart().startsWith("//"))
      .join("\n");
  }

  it("builds no `[resource]` line from a raw literal — every writer goes through taggedLine", () => {
    const offending = code()
      .split("\n")
      .map((l, i) => [i + 1, l] as const)
      .filter(([, l]) => /\[resource\]/.test(l))
      .map(([n, l]) => `${n}: ${l.trim()}`);
    expect(offending).toEqual([]);
    expect(code()).toContain('taggedLine("resource"');
  });
});
