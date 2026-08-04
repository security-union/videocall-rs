import { execFile } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";

import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);

/**
 * Subprocess regression lock for the orphaned-`ssh` defect (issue 2133,
 * BLOCKER 1) — the one shape no in-process test can express.
 *
 * Why a subprocess is REQUIRED here, not merely thorough:
 *
 * The bug was that `reapChild` `unref()`'d its SIGKILL grace timer. `unref()`
 * removes a timer from the event-loop refcount, so it only fires if something
 * ELSE keeps the process alive. The production caller
 * (`finalizeAll` → `orchestrator.ts:596` → `cli.ts:508`) calls
 * `process.exit(0)` immediately after awaiting, and `process.exit` runs no
 * pending timers — so the escalation never happened and a SIGTERM-ignoring
 * `ssh` was orphaned on the bots-app host.
 *
 * Every in-process escalation test was blind to this, because vitest keeps the
 * event loop alive for the whole run: any `await setTimeout(...)` in the test
 * body was itself the ref'd timer that let the unref'd grace timer fire. The
 * test passed; production leaked.
 *
 * This test therefore runs a fixture in a REAL child process that exits the way
 * `cli.ts` does, with an injected `bash -c 'trap "" TERM; sleep 300'` child that
 * ignores SIGTERM. After the fixture process is gone, the pid must be gone too.
 * On the un-fixed code the pid survives (verified).
 */

/** True while `pid` names a live process. Signal 0 probes without delivering. */
function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

describe("retrieve() reaps a SIGTERM-ignoring ssh even when the caller exits", () => {
  it("leaves no orphan after the calling process has exited", async () => {
    const fixture = resolve(import.meta.dirname, "fixtures", "exiting-retrieve-caller.ts");
    const outPath = join(mkdtempSync(join(tmpdir(), "bots-orphan-")), "raw.csv");

    // `npx tsx <fixture>` — a genuinely separate process that hits
    // `process.exit(0)` right after awaiting `retrieve`.
    const { stdout } = await execFileAsync("npx", ["tsx", fixture], {
      cwd: resolve(import.meta.dirname, "..", "..", ".."),
      env: { ...process.env, OUT_PATH: outPath, STALL_MS: "250", GRACE_MS: "300" },
      timeout: 90_000,
    });

    const m = /RETRIEVE_PID=(\d+)/.exec(stdout);
    expect(m, `fixture did not report a pid; stdout was:\n${stdout}`).not.toBeNull();
    const pid = Number(m?.[1]);

    // Every child the fixture spawned OTHER than the one under test. It is
    // responsible for reaping these itself (they are SIGTERM-immune `sleep 300`s,
    // so a leak would sit on the shared CI runner for 5 minutes each and
    // accumulate across runs).
    const others = (/OTHER_PIDS=([\d,]*)/.exec(stdout)?.[1] ?? "")
      .split(",")
      .filter((s) => s !== "")
      .map(Number);

    // The fixture process has exited by now (execFile resolved). Give the OS a
    // moment to finish delivering the SIGKILL the fixture issued before exiting.
    await new Promise((r) => setTimeout(r, 1500));

    // THE assertion: the SIGTERM-ignoring child must be gone. If the escalation
    // was dropped (unref'd timer, or `retrieve` not awaiting the reap), this pid
    // is still sitting in `sleep 300`.
    const survived = alive(pid);
    if (survived) {
      // Do not leak the orphan out of the test run if the assertion fails.
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        /* already gone */
      }
    }
    expect(
      survived,
      `pid ${pid} survived the caller's exit — the SIGKILL escalation was lost`,
    ).toBe(false);

    // And the fixture must not leak its OWN extras (the sampler-session child,
    // which is outside the scope of the code under test). Asserted, not assumed:
    // before this was fixed each run left SIGTERM-immune `sleep 300`s behind, and
    // this test runs in the `e2e` suite on the shared 16-vCPU CI runner where
    // they accumulate across runs.
    expect(others.length, "fixture should report the extra pids it spawned").toBeGreaterThan(0);
    const leaked = others.filter(alive);
    for (const p of leaked) {
      try {
        process.kill(p, "SIGKILL");
      } catch {
        /* already gone */
      }
    }
    expect(
      leaked,
      `the fixture leaked ${leaked.length} SIGTERM-immune process(es): ${leaked.join(", ")} — ` +
        `each would sit on the shared CI runner for 300s`,
    ).toEqual([]);
    // 90s cap: `npx tsx` cold-start dominates, so allow generous headroom.
  }, 120_000);
});
