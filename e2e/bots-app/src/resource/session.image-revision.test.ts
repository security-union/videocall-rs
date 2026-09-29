import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import type { SshHost } from "../control/ssh-hosts";
import { RemoteResourceManager, ResourceCaptureSession } from "./session";

const SHA = "c0ffee".padEnd(40, "0");
const REVISION_LINE = `[resource] image revision: ${SHA}\n`;

class PipingChild extends EventEmitter {
  readonly stdin = { write: vi.fn(), end: vi.fn() };
  readonly stderr = new EventEmitter();
  readonly stdout = new PassThrough();
  readonly kill = vi.fn(() => true);
  pid = 4322;
}

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

describe("run receipts and the image revision (#2293)", () => {
  afterEach(() => vi.unstubAllEnvs());

  it("the local capture's receipt carries this process's revision", async () => {
    vi.stubEnv("BOTS_IMAGE_REVISION", SHA);
    const s = new ResourceCaptureSession({
      runDir: mkdtempSync(join(tmpdir(), "bots-rev-local-")),
      label: "x",
    });
    mkdirSync(dirname(s.rawCsvPath), { recursive: true });
    writeFileSync(s.rawCsvPath, "ts,cpu_user\n1,2\n");
    const r = await s.finalize(new Map(), null, null);
    expect(r?.reportText).toContain(REVISION_LINE);
  });

  it("a remote host's receipt never carries this process's revision", async () => {
    vi.stubEnv("BOTS_IMAGE_REVISION", SHA);
    const child = new PipingChild();
    const mgr = new RemoteResourceManager({
      runDir: mkdtempSync(join(tmpdir(), "bots-rev-remote-")),
      label: "run-x",
      maxSeconds: 600,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: vi.fn(() => child as never) as never,
      retrieveStallMs: 500,
      retrieveKillGraceMs: 10,
    });
    await mgr.ensureForHost(host("lab-7"));
    const pending = mgr.finalizeAll();
    await new Promise((r) => setTimeout(r, 20));
    child.stdout.write("ts,cpu_user\n1,2\n");
    child.emit("exit", 0);
    child.emit("close", 0);
    const results = await pending;
    expect(results).toHaveLength(1);
    expect(results[0].reportText).toContain("[resource] image revision: not recorded");
    expect(results[0].reportText).not.toContain(SHA);
  });
});
