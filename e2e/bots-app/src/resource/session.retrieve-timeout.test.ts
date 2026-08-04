import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { mkdtempSync, readdirSync, readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it, vi } from "vitest";

import type { SshHost } from "../control/ssh-hosts";
import {
  RemoteResourceManager,
  RETRIEVE_KILL_GRACE_MS,
  RETRIEVE_STALL_TIMEOUT_MS,
  SSH_KEEPALIVE_ARGS,
  startRemoteSampler,
} from "./session";

/**
 * Regression lock for the inactivity bound on the remote CSV retrieve (issue
 * 2133 sweep — the same defect shape as the SSH host probe).
 *
 * The bug: `retrieve()` spawned `ssh … cat <remoteCsvPath>` with
 * `connectTimeout: 10` + `SSH_KEEPALIVE_ARGS` and NO bound on the transfer
 * itself. The keepalives cover a dropped TRANSPORT (~15s) only; a remote box
 * whose sshd keeps answering keepalives while `cat` never completes (wedged
 * filesystem, saturated box, hung NFS read) left the session alive and silent
 * forever. `finalizeAll()` awaits `handle.retrieve(...)` per host, so ONE wedged
 * host stalled the whole run's teardown and cost the run its resource CSVs.
 *
 * The stub emits nothing and never exits, so on the un-fixed code the promise
 * has no path to resolution and these tests hang until vitest kills them —
 * mutation sensitivity is structural.
 *
 * The load-bearing case is `does NOT sever a slow but PROGRESSING transfer`:
 * without it an absolute deadline and an inactivity deadline are
 * indistinguishable in test, and an absolute one would truncate a long run's
 * (legitimately large) CSV.
 */

/** Child stand-in whose stdout/exit are driven by the test. */
class StallingChild extends EventEmitter {
  readonly stdin = { write: vi.fn(), end: vi.fn() };
  readonly stderr = new EventEmitter();
  readonly stdout = Object.assign(new EventEmitter(), { pipe: vi.fn() });
  readonly signals: string[] = [];
  readonly kill = vi.fn((signal?: string) => {
    this.signals.push(signal ?? "SIGTERM");
    return true;
  });
  pid = 4321;

  /** Emit one stdout chunk (advances `bytes` and rearms the stall timer). */
  chunk(text: string): void {
    this.stdout.emit("data", Buffer.from(text, "utf8"));
  }
}

/**
 * Child whose stdout is a REAL `Readable`, so `stdout.pipe(out)` actually writes
 * into the WriteStream. Needed for the write/flush-failure cases — the
 * `vi.fn()` `pipe` on {@link StallingChild} is a no-op, so no bytes ever reach
 * the file and a flush error can never be provoked.
 */
class PipingChild extends EventEmitter {
  readonly stdin = { write: vi.fn(), end: vi.fn() };
  readonly stderr = new EventEmitter();
  readonly stdout = new PassThrough();
  readonly signals: string[] = [];
  readonly kill = vi.fn((signal?: string) => {
    this.signals.push(signal ?? "SIGTERM");
    return true;
  });
  pid = 4322;

  /** Push one chunk through the real pipe. */
  chunk(text: string): void {
    this.stdout.write(Buffer.from(text, "utf8"));
  }
}

/**
 * A child that exits when SIGTERMed, like a responsive `ssh`. Used to pin that
 * the escalation does NOT fire in that case.
 */
class ExitOnTermChild extends StallingChild {
  constructor() {
    super();
    this.kill.mockImplementation((signal?: string) => {
      this.signals.push(signal ?? "SIGTERM");
      if (signal === "SIGTERM")
        // Emit BOTH, in the real order: a dying child fires `exit` (process gone)
        // and then `close` (stdio drained). `retrieve` settles on `close`.
        queueMicrotask(() => {
          this.emit("exit", null);
          this.emit("close", null);
        });
      return true;
    });
  }
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

/** Count of live `setTimeout` handles — proves no dangling timer is left. */
function pendingTimers(): number {
  return process.getActiveResourcesInfo().filter((r) => r === "Timeout").length;
}

/**
 * Open file descriptors for this process. Linux-only (`/proc/self/fd`), which is
 * where CI runs; on any other platform this returns -1 and the fd assertions
 * degrade to trivially-true rather than failing spuriously.
 */
function openFdCount(): number {
  try {
    return readdirSync("/proc/self/fd").length;
  } catch {
    return -1;
  }
}

function sampler(
  child: StallingChild | PipingChild,
  over: { stallMs?: number; killGraceMs?: number } = {},
) {
  // First spawn is the sampler session, second is the `cat` in retrieve(). The
  // same stub child serves both — retrieve() is the only one we drive.
  const spawn = vi.fn(() => child as never);
  const handle = startRemoteSampler("#!/usr/bin/env bash\n", {
    host: host("lab-7"),
    maxSeconds: 600,
    spawn: spawn as never,
    retrieveStallMs: over.stallMs ?? 40,
    retrieveKillGraceMs: over.killGraceMs ?? 10,
  });
  return { handle, spawn };
}

function outPath(): string {
  return join(mkdtempSync(join(tmpdir(), "bots-retrieve-")), "raw.csv");
}

/**
 * ssh's transport-silence teardown budget, DERIVED from the argv production
 * actually passes rather than recomputed as `5 * 3 * 1000` — otherwise retuning
 * the keepalives would silently invalidate the assertions below.
 */
function serverAliveBudgetMs(args: readonly string[]): number {
  const num = (key: string): number => {
    const hit = args.find((a) => a.startsWith(`${key}=`));
    expect(hit, `keepalive argv is missing ${key}`).toBeDefined();
    return Number((hit as string).split("=")[1]);
  };
  return num("ServerAliveInterval") * num("ServerAliveCountMax") * 1000;
}

describe("retrieve() inactivity bound (issue 2133 sweep)", () => {
  it("resolves 0 at the stall deadline instead of hanging finalizeAll forever", async () => {
    const child = new StallingChild();
    const { handle } = sampler(child);

    const start = performance.now();
    const bytes = await handle.retrieve(outPath());
    const elapsed = performance.now() - start;

    // Best-effort contract preserved: 0 bytes means "no CSV", never a throw.
    // `finalizeAll` already treats 0 as a skip-with-warning.
    expect(bytes).toBe(0);
    expect(elapsed).toBeLessThan(2000);
  });

  it("actually KILLS the ssh child rather than abandoning it", async () => {
    const child = new StallingChild();
    const { handle } = sampler(child);

    await handle.retrieve(outPath());

    // Resolving is not enough — the leaked `ssh` per wedged host IS the bug.
    expect(child.kill).toHaveBeenCalled();
    expect(child.signals[0]).toBe("SIGTERM");
  });

  it("escalates to SIGKILL BEFORE resolving — no competing test timer needed", async () => {
    // This assertion shape is the whole point, and the previous version of this
    // test was structurally blind to a real defect.
    //
    // The old form asserted `["SIGTERM"]` at resolution, then did
    // `await setTimeout(80)` and asserted the SIGKILL had landed. That PASSED
    // even when the grace timer was `unref`'d — because the test's own
    // (ref'd) 80ms timer was what kept the event loop alive long enough for the
    // unref'd timer to fire. In production nothing plays that role:
    // `finalizeAll` → `orchestrator.ts:596` → `cli.ts:508` calls
    // `process.exit(0)`, which runs no pending timers, so the escalation was
    // silently dropped and the wedged `ssh` was orphaned.
    //
    // Asserting the FULL sequence has completed by the time the awaited promise
    // settles removes the crutch: `retrieve` must await the reap itself.
    const child = new StallingChild();
    const { handle } = sampler(child, { stallMs: 30, killGraceMs: 20 });

    await handle.retrieve(outPath());

    // No `await setTimeout(...)` between the await above and this assertion.
    expect(child.signals).toEqual(["SIGTERM", "SIGKILL"]);
  });

  it("does NOT escalate when the child exits on SIGTERM", async () => {
    const child = new ExitOnTermChild();
    const { handle } = sampler(child, { stallMs: 30, killGraceMs: 20 });

    await handle.retrieve(outPath());

    // The `exit` handler must cancel the reaper, else we SIGKILL a pid the OS
    // may already have recycled. Also asserted with no intervening timer: the
    // reap resolves via `cancel()`, so the await above is sufficient.
    expect(child.signals).toEqual(["SIGTERM"]);
    // And it must not appear later either.
    await new Promise((r) => setTimeout(r, 60));
    expect(child.signals).toEqual(["SIGTERM"]);
  });

  it("does NOT sever a slow but PROGRESSING transfer (inactivity, not absolute)", async () => {
    // THE case that distinguishes an inactivity bound from an absolute one. The
    // transfer runs for ~6x the stall window in total, but never goes quiet for
    // a full window, so it must complete untouched. Under an absolute deadline
    // this transfer would be severed mid-stream and the CSV lost — exactly the
    // failure a long run (bigger CSV) would hit in production.
    const child = new StallingChild();
    const { handle } = sampler(child, { stallMs: 50, killGraceMs: 10 });

    const path = outPath();
    const promise = handle.retrieve(path);

    // 6 chunks at 30ms spacing = ~180ms elapsed, each gap (30ms) under the
    // 50ms stall window.
    for (let i = 0; i < 6; i++) {
      await new Promise((r) => setTimeout(r, 30));
      child.chunk("row\n");
    }
    child.emit("exit", 0);
    child.emit("close", 0);

    const bytes = await promise;
    // 6 chunks x 4 bytes — the FULL transfer, not a truncated prefix.
    expect(bytes).toBe(24);
    expect(child.kill).not.toHaveBeenCalled();
  });

  it("returns the byte count on a healthy transfer and leaves no pending timer", async () => {
    const child = new StallingChild();
    // A LONG stall window: if the success path failed to clear it, the timer
    // would still be armed after the promise resolves.
    const { handle } = sampler(child, { stallMs: 600_000, killGraceMs: 10 });

    const before = pendingTimers();
    const promise = handle.retrieve(outPath());
    child.chunk("cpu,mem\n");
    child.chunk("1,2\n");
    child.emit("exit", 0);
    child.emit("close", 0);
    const bytes = await promise;

    expect(bytes).toBe(12);
    expect(child.kill).not.toHaveBeenCalled();
    // A dangling `setTimeout` keeps the orchestrator's event loop alive at the
    // very end of a run — the run would appear to hang after printing.
    expect(pendingTimers()).toBe(before);
  });

  it("reports 0 (not a crash) when the local CSV path is unwritable", async () => {
    // A WriteStream `error` is emitted ASYNCHRONOUSLY, so it does NOT reach
    // `finalizeAll`'s try/catch: with no `error` listener it becomes an
    // uncaughtException and kills the orchestrator at run end — on a path
    // documented as best-effort. Verified: node raises
    // `Unhandled 'error' event` for exactly this case.
    const child = new StallingChild();
    const { handle } = sampler(child, { stallMs: 600_000, killGraceMs: 10 });

    const bytes = await handle.retrieve("/nonexistent-dir-2133/raw.csv");

    expect(bytes).toBe(0);
  });

  it("closes the write stream on the stall path (no leaked file descriptor)", async () => {
    // Resolving without `out.end()` leaks a descriptor per wedged host. This
    // asserts on the OS-visible fd count, NOT on the file existing: `pipe()`
    // writes eagerly, so the file is present either way — an existence check
    // passes even with the `out.end()` removed and proves nothing (verified:
    // that version of this test survived the mutation).
    const child = new StallingChild();
    const { handle } = sampler(child, { stallMs: 40, killGraceMs: 10 });

    const before = openFdCount();
    const path = outPath();
    const promise = handle.retrieve(path);
    child.stdout.emit("data", Buffer.from("partial\n", "utf8"));
    const bytes = await promise;

    expect(bytes).toBe(0); // stalled → "no usable CSV"
    // Give the close a tick to land, then require the fd table to be back where
    // it started.
    await new Promise((r) => setTimeout(r, 50));
    expect(openFdCount()).toBe(before);
  });

  it("unblocks finalizeAll() — the caller whose hang motivated the fix", async () => {
    // The bound must be reachable from the REAL entry point, not just from a
    // hand-built handle: `finalizeAll()` awaits `handle.retrieve(...)` per host,
    // which is where one wedged host stalled the whole run's teardown.
    const child = new StallingChild();
    const spawn = vi.fn(() => child as never);
    const mgr = new RemoteResourceManager({
      runDir: mkdtempSync(join(tmpdir(), "bots-finalize-")),
      label: "run-x",
      maxSeconds: 600,
      scriptText: "#!/usr/bin/env bash\n",
      spawn: spawn as never,
      retrieveStallMs: 40,
      retrieveKillGraceMs: 10,
    });
    await mgr.ensureForHost(host("lab-7"));

    const start = performance.now();
    const results = await mgr.finalizeAll();
    const elapsed = performance.now() - start;

    // Returns (no CSV for the wedged host) instead of hanging, and the ssh
    // child is reaped rather than orphaned.
    expect(results).toEqual([]);
    expect(elapsed).toBeLessThan(2000);
    expect(child.kill).toHaveBeenCalled();
  });

  it("returns the FULL byte count for a realistic CSV that reaches EOF via pipe", async () => {
    // THE happy path, and the case the first FIX-6 attempt broke deterministically
    // on every successful transfer.
    //
    // `stdout.pipe(out)` AUTO-ENDS `out` at source EOF. So on the normal path the
    // stream is already finished by the time `settle` runs, and `settle`'s
    // belt-and-braces `out.end(cb)` is a SECOND end whose callback receives
    // `ERR_STREAM_ALREADY_FINISHED`. Treating any `err` as a flush failure meant
    // a COMPLETE CSV was reported as 0 bytes — and `finalizeAll` drops a 0-byte
    // host (`if (bytes === 0) { warn; continue; }`), so the run silently lost the
    // very artifact #2133 exists to protect. Verified: 25239 bytes on disk,
    // `retrieve` returned 0.
    //
    // The ENOSPC test below could not see it, because /dev/full fails BEFORE EOF
    // so the fixture's PassThrough never ends and `settle`'s end() is the FIRST
    // end. Hence this test: a realistic multi-chunk sampler CSV driven to EOF
    // through a REAL pipe, exactly as production does.
    const child = new PipingChild();
    const { handle } = sampler(child, { stallMs: 600_000, killGraceMs: 10 });

    const path = outPath();
    const promise = handle.retrieve(path);

    // 720 rows ≈ a 1-hour run at the default 5s cadence — multi-chunk and well
    // past the stream's internal highWaterMark, so the flush is real.
    const header = "ts,cpu_user,cpu_sys,rss_kb,nproc,label\n";
    let expected = Buffer.byteLength(header);
    child.chunk(header);
    for (let i = 0; i < 720; i++) {
      const row = `${1785600000 + i * 5},12.5,3.1,${480000 + i},7,run-x\n`;
      expected += Buffer.byteLength(row);
      child.chunk(row);
    }
    child.stdout.end(); // EOF — this is what makes `pipe` end `out`
    await new Promise((r) => setTimeout(r, 50));
    child.emit("exit", 0);
    child.emit("close", 0);

    const bytes = await promise;

    // The FULL length, not 0 and not a truncated prefix.
    expect(bytes).toBe(expected);
    // …and the bytes really are on disk, so a caller acting on the count is safe.
    expect(statSync(path).size).toBe(expected);
    expect(readFileSync(path, "utf8").startsWith(header)).toBe(true);
    expect(child.kill).not.toHaveBeenCalled();
  });

  it("does NOT truncate when the process dies before the pipe has drained", async () => {
    // The defect the test ABOVE cannot see, and the reason `retrieve` settles on
    // `close` rather than `exit`.
    //
    // `exit` means the child PROCESS died; its stdio may still be draining.
    // `close` means the process died AND stdio was fully consumed. Settling on
    // `exit` therefore ran `settle()` mid-pipe, so `closeOut` saw
    // `writableEnded === false` and called `out.end()` on a stream the pipe was
    // still writing to — TRUNCATING the file. The pipe's next write then raised
    // `ERR_STREAM_WRITE_AFTER_END` into the pending `end` callback, which the
    // benign-code whitelist reported as SUCCESS: a short CSV returned as
    // complete, and `finalizeAll` deriving a verdict from partial data. That is
    // strictly worse than the earlier bug, which at least failed loudly at 0.
    //
    // Measured against a slow sink (what a real disk/NFS write is): at `exit`,
    // 1,785,856 of 2,000,000 bytes were written with `writableEnded === false`;
    // at `close`, all 2,000,000 with `writableEnded === true`.
    //
    // THE ABSENCE OF A GRACE DELAY IS THE POINT. The happy-path test above waits
    // 50ms before `exit`, which lets the pipe finish and forces `closeOut` down
    // its already-finished branch — so it passes either way. Here `exit` fires in
    // the SAME TICK as EOF, with bytes still in flight, which is what production
    // does on a slow disk and what no other test reproduces.
    const child = new PipingChild();
    const { handle } = sampler(child, { stallMs: 600_000, killGraceMs: 10 });

    const path = outPath();
    const promise = handle.retrieve(path);

    // Large enough that the write cannot complete synchronously.
    const header = "ts,cpu_user,cpu_sys,rss_kb,nproc,label\n";
    let expected = Buffer.byteLength(header);
    child.chunk(header);
    for (let i = 0; i < 2000; i++) {
      const row = `${1785600000 + i * 5},12.5,3.1,${480000 + i},7,run-slow-disk\n`;
      expected += Buffer.byteLength(row);
      child.chunk(row);
    }
    // THE ORDERING, faithful to a real ChildProcess: `exit` fires as soon as the
    // PROCESS dies, while its stdout is still being consumed; `close` fires only
    // once stdio has drained. Node guarantees that order, and it guarantees that
    // by `close` the pipe has already called `out.end()` (source `end` -> pipe
    // ends the destination, and stdout's `close` follows its `end`) — which is
    // precisely why settling on `close` can never land mid-pipe.
    //
    // So: process dies here, with ~86 KB still in flight…
    child.emit("exit", 0);
    child.stdout.end();
    // …and `close` arrives only once stdout has actually been consumed. Keying
    // off stdout's own `end` event rather than a guessed number of ticks is what
    // makes this faithful: 86 KB does not drain in one `setImmediate`, and a
    // hand-tuned delay would silently become a grace period like the one that
    // makes the test above blind to this bug.
    await new Promise((r) => child.stdout.once("end", r));
    child.emit("close", 0);

    const bytes = await promise;

    // MUTATION SENSITIVITY: change the listener back to `exit` and this fails —
    // `settle` runs while bytes are still in flight, `out.end()` truncates, and
    // the result is short (verified: 65571 of 86039). With the WRITE_AFTER_END
    // whitelist entry ALSO restored, `bytes` instead lies and the tail assert
    // below is what catches it.
    expect(bytes).toBe(expected);
    expect(statSync(path).size).toBe(expected);
    // The LAST row must be present — a truncated flush loses the tail, and a
    // byte count alone could look right while the content is short.
    expect(readFileSync(path, "utf8").endsWith("run-slow-disk\n")).toBe(true);
  });

  it("reports 0 — not a false success — when the flush fails AFTER a clean exit", async () => {
    // Uses a child whose stdout is a REAL Readable, so `pipe()` genuinely writes
    // into the WriteStream. The `StallingChild` stub's `pipe` is a `vi.fn()`
    // no-op, which cannot exercise a flush at all — with it, this test passed
    // trivially against the un-fixed code (verified) and proved nothing.
    // Codex P2 / FIX 6, and worse than the crash it sits next to: silent bad data
    // instead of a loud failure.
    //
    // Ordering that produces it: the ssh child exits 0, so the `exit` handler
    // settles with the byte count and calls `out.end()`. The ENOSPC/EDQUOT only
    // materialises while FLUSHING — after `settled` is already true, so the
    // `out.on("error")` guard returns early. Pre-fix, the `end` callback then
    // resolved with `bytes`, and `finalizeAll` went on to parse a truncated CSV
    // believing the transfer succeeded.
    //
    // `/dev/full` accepts the open and the buffered write, then fails the flush —
    // and (verified) the `end` callback receives ENOSPC BEFORE the `error` event
    // fires, which is exactly why the error has to be handled in the callback.
    const child = new PipingChild();
    const { handle } = sampler(child, { stallMs: 600_000, killGraceMs: 10 });

    const promise = handle.retrieve("/dev/full");
    // Write and exit in the SAME TICK. This ordering is essential and is the
    // whole difference between exercising the bug and not:
    //   - If the `error` event gets a tick to fire FIRST, the `out.on("error")`
    //     handler settles the promise and the flush path is never reached (that
    //     version of this test passed even with the fix reverted — verified).
    //   - Same-tick, `exit` settles with the byte count and the ENOSPC surfaces
    //     only while `end()` flushes, which is exactly the production race: a
    //     late write error after a clean child exit.
    child.chunk("ts,cpu\n1,2\n");
    child.emit("exit", 0); // clean exit — pre-fix this reported 11 bytes
    child.emit("close", 0);
    const bytes = await promise;

    // 0 = "no usable CSV", so finalizeAll skips it with a warning instead of
    // deriving a verdict from a partial file.
    expect(bytes).toBe(0);
  });

  it("exports production defaults sized for a CSV transfer, not a probe", async () => {
    // Pins that the default is a real value rather than something only tests
    // set, and that it clears ssh's own ~15s ServerAlive teardown so a broken
    // TRANSPORT is reported by ssh (better diagnostic) rather than by this
    // timer.
    expect(RETRIEVE_STALL_TIMEOUT_MS).toBe(20_000);
    expect(RETRIEVE_STALL_TIMEOUT_MS).toBeGreaterThan(serverAliveBudgetMs(SSH_KEEPALIVE_ARGS));
    expect(RETRIEVE_KILL_GRACE_MS).toBeGreaterThan(0);
    expect(RETRIEVE_KILL_GRACE_MS).toBeLessThan(RETRIEVE_STALL_TIMEOUT_MS);
    // The probe's bound is ABSOLUTE and this one is INACTIVITY, so they are not
    // directly comparable — but both must clear the same ~15s ServerAlive floor.
    const { SSH_PROBE_TIMEOUT_MS } = await import("../control/ssh-hosts");
    expect(SSH_PROBE_TIMEOUT_MS).toBeGreaterThan(serverAliveBudgetMs(SSH_KEEPALIVE_ARGS));
  });
});
