/**
 * Fixture for the orphan-reap subprocess test (issue 2133 / BLOCKER 1).
 *
 * Mirrors the REAL production caller shape that no in-process test can express:
 * `finalizeAll` → `orchestrator.ts:596` → `cli.ts:508` awaits the retrieve and
 * then calls `process.exit(0)` unconditionally. `process.exit` does not run
 * pending timers, so if the SIGKILL escalation is `unref`'d — or if `retrieve`
 * does not await it — the wedged `ssh` survives as an orphan.
 *
 * The injected child is `trap "" TERM; sleep 300`: it IGNORES SIGTERM, exactly
 * like an `ssh` blocked in a syscall, so only a real SIGKILL can end it.
 *
 * Prints `RETRIEVE_PID=<pid>` (the child under test) and `OTHER_PIDS=<csv>` (every
 * other child this fixture spawned) so the parent test can check for an orphan
 * after this process is gone AND assert nothing else leaked. Run via `tsx`, not
 * vitest — the point is a process that genuinely exits.
 *
 * Every child here is `trap "" TERM; sleep 300`, i.e. SIGTERM-IMMUNE by design.
 * Only the `cat` child is reaped by the code under test; the sampler-session
 * child is not, so this fixture MUST SIGKILL its own extras before exiting or it
 * leaks SIGTERM-immune processes for 300s each — and this test runs in the `e2e`
 * vitest suite on the shared 16-vCPU CI runner, where they would accumulate
 * across runs.
 */
import { spawn as realSpawn } from "node:child_process";

import type { SshHost } from "../../control/ssh-hosts";
import { startRemoteSampler } from "../session";

const host: SshHost = {
  label: "wedged",
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

async function main(): Promise<void> {
  const pids: number[] = [];
  const spawn = ((_cmd: string, _args: string[], _o: unknown) => {
    // `exec` matters: without it `bash -c` stays resident and `sleep` is a
    // GRANDCHILD, so killing the pid we recorded orphans the sleep (verified —
    // that is how the first version of this fixture leaked). `exec` replaces the
    // shell with `sleep`, giving ONE process, and the SIGTERM-ignore disposition
    // set by `trap ""` is INHERITED across exec (verified: it still survives a
    // SIGTERM), so the child is still a faithful stand-in for a syscall-blocked
    // `ssh`. Short-ish sleep as a second line of defence: even if this fixture is
    // killed before its own cleanup runs, a stray child expires in 30s rather
    // than 300s.
    const child = realSpawn("bash", ["-c", 'trap "" TERM; exec sleep 30'], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    pids.push(child.pid as number);
    return child;
  }) as never;

  const handle = startRemoteSampler("#!/usr/bin/env bash\n", {
    host,
    maxSeconds: 60,
    spawn,
    retrieveStallMs: Number(process.env.STALL_MS ?? 250),
    retrieveKillGraceMs: Number(process.env.GRACE_MS ?? 300),
  });
  await handle.retrieve(process.env.OUT_PATH as string);
  // The `cat` child is the LAST spawned (the first is the sampler session).
  const retrievePid = pids[pids.length - 1];
  const others = pids.slice(0, -1);
  console.log(`RETRIEVE_PID=${retrievePid}`);
  console.log(`OTHER_PIDS=${others.join(",")}`);
  // Reap OUR OWN extras (the sampler session). These are outside the scope of
  // the code under test — `retrieve` only owns the `cat` child — so leaving them
  // is a fixture leak, not a product bug. SIGKILL directly: they trap SIGTERM,
  // so a polite signal would be ignored and `process.exit` below runs no timers.
  for (const pid of others) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // Already gone.
    }
  }
  process.exit(0);
}

void main();
