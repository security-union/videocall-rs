import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, vi } from "vitest";

import {
  addHost,
  buildSshArgsForProbe,
  runSshProbe,
  PROBE_KEEPALIVE_ARGS,
  SSH_PROBE_KILL_GRACE_MS,
  SSH_PROBE_TIMEOUT_MS,
  testHost,
  type SshHost,
} from "./ssh-hosts";

/**
 * Regression lock for the post-connect bound on the SSH host probe (issue 2133).
 *
 * The bug: `buildBaseSshArgs` sets `ConnectTimeout=5`, which bounds ONLY the TCP
 * connect phase. Nothing bounded the session once TCP was established, so a host
 * that completes the handshake and then stalls (wedged sshd, saturated box, hung
 * PAM/LDAP, filesystem stall on login) left the spawned `ssh` running forever:
 * `POST /hosts/:label/test` never responded, the dashboard "Test" button span
 * indefinitely, and repeated probes accumulated orphaned `ssh` processes.
 *
 * The stub below is the faithful stand-in for that host: `spawn` succeeds (the
 * connect phase worked), then the child emits NOTHING — no stdout, no stderr,
 * and critically never `exit`. On the un-fixed code the returned promise has no
 * path to resolution at all, so these tests hang until vitest kills them and
 * FAIL. Mutation sensitivity is therefore structural, not incidental.
 *
 * Real ssh is never forked: every case drives the production `runSshProbe` /
 * `testHost` through the `TestHostDeps` seam, injecting a short `timeoutMs` so
 * the suite does not wait out the 30s production default.
 */

/** Records every signal a probe sends so the tests can assert on the reaping. */
interface StubChild {
  kill: ReturnType<typeof vi.fn>;
  signals: string[];
  /** Simulate the child exiting (what a responsive ssh does on SIGTERM). */
  fireExit(code: number | null): void;
}

/**
 * A `spawn` stand-in whose child is configurable along the one axis that
 * matters: whether it ever exits.
 *
 * - `mode: "stall"`   — connects, then total silence. Never exits, and IGNORES
 *                       SIGTERM, which is what forces the SIGKILL escalation
 *                       (an `ssh` blocked in a syscall behaves exactly so).
 * - `mode: "healthy"` — emits the probe sentinel on stdout then exits 0.
 * - `mode: "exit-on-term"` — silent until signalled, then exits on SIGTERM like
 *                       a responsive ssh; used to pin that we do NOT escalate.
 */
function stubSpawn(opts: { mode: "stall" | "healthy" | "exit-on-term" }): {
  spawn: ReturnType<typeof vi.fn>;
  child: StubChild;
} {
  const signals: string[] = [];
  const exitHandlers: Array<(code: number | null) => void> = [];
  const stdoutHandlers: Array<(b: Buffer) => void> = [];

  const fireExit = (code: number | null): void => {
    for (const h of exitHandlers) h(code);
  };

  const kill = vi.fn((signal?: string) => {
    signals.push(signal ?? "SIGTERM");
    // A responsive ssh exits when TERMed; the "stall" child deliberately does
    // not, so the probe must escalate.
    if (opts.mode === "exit-on-term" && signal === "SIGTERM") {
      queueMicrotask(() => fireExit(null));
    }
    return true;
  });

  const spawn = vi.fn(() => {
    const child = {
      stdout: {
        on: (event: string, cb: (b: Buffer) => void) => {
          if (event === "data") stdoutHandlers.push(cb);
        },
      },
      stderr: {
        on: (_event: string, _cb: (b: Buffer) => void) => {
          // The stalled + healthy hosts never write to stderr.
        },
      },
      on: (event: string, cb: (...args: unknown[]) => void) => {
        if (event === "exit") exitHandlers.push(cb as (code: number | null) => void);
        // `error` is intentionally not wired: spawn SUCCEEDED in every case
        // here. This is a post-connect stall, not a spawn failure.
      },
      kill,
    };
    if (opts.mode === "healthy") {
      // Fire on the next microtask so the listeners are registered first.
      queueMicrotask(() => {
        for (const h of stdoutHandlers)
          h(Buffer.from("bots-app-probe-ok\nLinux box 5.15\n", "utf8"));
        fireExit(0);
      });
    }
    return child;
  });

  return { spawn: spawn, child: { kill, signals, fireExit } };
}

/**
 * Count of live `setTimeout` handles holding the event loop open. Used to prove
 * the probe leaves no dangling deadline behind — a leaked timer would keep the
 * ctl server's process alive on shutdown.
 */
function pendingTimers(): number {
  return process.getActiveResourcesInfo().filter((r) => r === "Timeout").length;
}

function host(over: Partial<SshHost> = {}): SshHost {
  return {
    label: "wedged",
    host: "stalled.intra",
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

/**
 * ssh's own transport-silence teardown budget, DERIVED from the argv the
 * production code actually passes (`ServerAliveInterval` x `ServerAliveCountMax`).
 * Computed rather than hardcoded as `5 * 3 * 1000`: recomputing the product from
 * memory means retuning the keepalive args would silently invalidate every
 * assertion that depends on this floor.
 */
function serverAliveBudgetMs(args: readonly string[]): number {
  const num = (key: string): number => {
    const hit = args.find((a) => a.startsWith(`${key}=`));
    expect(hit, `probe argv is missing ${key}`).toBeDefined();
    return Number((hit as string).split("=")[1]);
  };
  return num("ServerAliveInterval") * num("ServerAliveCountMax") * 1000;
}

describe("runSshProbe post-connect timeout (issue 2133)", () => {
  it("resolves ok=false at the bounded deadline instead of hanging forever", async () => {
    const { spawn } = stubSpawn({ mode: "stall" });

    const start = performance.now();
    const result = await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 40,
      killGraceMs: 10,
    });
    const elapsed = performance.now() - start;

    expect(result.ok).toBe(false);
    // The message must NAME the timeout so an operator reading the dashboard
    // can tell "wedged after connect" apart from "auth refused".
    expect(result.error).toMatch(/timed out after 40ms/);
    // Bounded by the injected deadline, NOT the production 30s default and
    // certainly not forever.
    expect(elapsed).toBeLessThan(2000);
  });

  it("actually KILLS the child rather than abandoning it (no orphaned ssh)", async () => {
    const { spawn, child } = stubSpawn({ mode: "stall" });

    await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 40,
      killGraceMs: 10,
    });

    // Resolving the promise is NOT enough — the whole point of the issue is the
    // leaked process, so assert the signal was really sent to the child.
    expect(child.kill).toHaveBeenCalled();
    expect(child.signals[0]).toBe("SIGTERM");
  });

  it("escalates to SIGKILL when the child ignores SIGTERM", async () => {
    const { spawn, child } = stubSpawn({ mode: "stall" });

    await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 30,
      killGraceMs: 20,
    });

    // At resolution only the TERM has been sent; the KILL is scheduled behind
    // the grace period.
    expect(child.signals).toEqual(["SIGTERM"]);
    await new Promise((r) => setTimeout(r, 80));
    // The stub never exits, so the escalation must fire. Without it a
    // syscall-blocked ssh survives indefinitely.
    expect(child.signals).toEqual(["SIGTERM", "SIGKILL"]);
  });

  it("does NOT escalate to SIGKILL when the child exits on SIGTERM", async () => {
    const { spawn, child } = stubSpawn({ mode: "exit-on-term" });

    await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 30,
      killGraceMs: 20,
    });

    await new Promise((r) => setTimeout(r, 80));
    // The `exit` handler must clear the pending kill timer — otherwise we
    // signal a pid the OS may already have recycled.
    expect(child.signals).toEqual(["SIGTERM"]);
  });

  it("leaves a healthy probe untouched: ok=true, no kill, no pending timer", async () => {
    const { spawn, child } = stubSpawn({ mode: "healthy" });

    // A LONG deadline: if the success path failed to clear it, the timer would
    // still be armed after the probe resolves. That is measurable directly (see
    // the handle-count assertion below) rather than inferred.
    const before = pendingTimers();
    const result = await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 600_000,
      killGraceMs: 10,
    });

    expect(result.ok).toBe(true);
    expect(result.output).toContain("bots-app-probe-ok");
    expect(child.kill).not.toHaveBeenCalled();
    // A dangling `setTimeout` keeps the Node event loop alive, which would hang
    // the ctl server on shutdown — as real a bug as the unbounded probe itself.
    expect(pendingTimers()).toBe(before);
  });

  it("clears the deadline on the spawn-`error` path too (no leaked timer)", async () => {
    // The `error` event is the one settle path with NO subsequent `exit`, so it
    // is the case where `settle`'s own `clearTimers` is load-bearing: the exit
    // handler is never reached to clean up on its behalf.
    const kill = vi.fn(() => true);
    const spawn = vi.fn(() => ({
      stdout: { on: () => {} },
      stderr: { on: () => {} },
      on: (event: string, cb: (err: Error) => void) => {
        if (event === "error") queueMicrotask(() => cb(new Error("ENOENT: ssh not found")));
      },
      kill,
    }));

    const before = pendingTimers();
    const result = await runSshProbe(host(), {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 600_000,
      killGraceMs: 10,
    });

    expect(result.ok).toBe(false);
    expect(result.error).toContain("ENOENT");
    expect(pendingTimers()).toBe(before);
  });

  it("bounds the probe through testHost() too (the route's actual entry point)", async () => {
    const dir = mkdtempSync(join(tmpdir(), "bots-probe-timeout-"));
    await addHost(dir, {
      label: "wedged",
      host: "stalled.intra",
      user: "alice",
      reposPath: "/home/alice/videocall",
    });
    const { spawn, child } = stubSpawn({ mode: "stall" });

    // `POST /hosts/:label/test` calls `testHost`, so the bound has to be
    // reachable from there — not just from the lower-level `runSshProbe`.
    const result = await testHost(dir, "wedged", {
      spawn: spawn as unknown as typeof import("node:child_process").spawn,
      timeoutMs: 40,
      killGraceMs: 10,
    });

    expect(result.ok).toBe(false);
    expect(result.error).toMatch(/timed out after 40ms/);
    expect(child.kill).toHaveBeenCalled();
  });

  it("defaults the bound to the exported production constants", () => {
    // Pins that the production default is a real, WAN-sane budget rather than
    // something only the tests set. Referenced (not hardcoded) so retuning the
    // constant does not silently invalidate the test above.
    expect(SSH_PROBE_TIMEOUT_MS).toBe(30_000);
    // Must sit above ssh's own ~15s ServerAlive teardown so the better
    // diagnostic wins whenever the transport is what broke.
    expect(SSH_PROBE_TIMEOUT_MS).toBeGreaterThan(serverAliveBudgetMs(PROBE_KEEPALIVE_ARGS));
    expect(SSH_PROBE_KILL_GRACE_MS).toBeGreaterThan(0);
    expect(SSH_PROBE_KILL_GRACE_MS).toBeLessThan(SSH_PROBE_TIMEOUT_MS);
  });
});

describe("buildSshArgsForProbe keepalive (issue 2133)", () => {
  it("carries ServerAlive options so ssh tears down an unresponsive session", () => {
    const args = buildSshArgsForProbe(host());
    // Cheap pin that the argv half of the fix did not get reverted: without
    // these, a silently-dropped transport hangs until the wall-clock backstop.
    expect(args).toContain("ServerAliveInterval=5");
    expect(args).toContain("ServerAliveCountMax=3");
    // ~15s of transport silence before ssh gives up — below the 30s backstop.
    expect(serverAliveBudgetMs(args)).toBeLessThan(SSH_PROBE_TIMEOUT_MS);
  });

  it("keeps the ServerAlive options BEFORE the remote command argv slot", () => {
    const args = buildSshArgsForProbe(host());
    const lastOpt = Math.max(
      args.indexOf("ServerAliveInterval=5"),
      args.indexOf("ServerAliveCountMax=3"),
    );
    // The remote command must stay the final slot: `ssh` treats the first
    // non-option argument as the destination and everything after as the
    // command, so an `-o` landing after it would be sent to the remote shell
    // instead of configuring ssh.
    expect(args[args.length - 1]).toContain("bots-app-probe-ok");
    expect(lastOpt).toBeLessThan(args.length - 1);
  });
});
