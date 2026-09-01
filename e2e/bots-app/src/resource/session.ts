/**
 * Runtime that forks the resource sampler for a run's duration and produces the
 * per-run artifacts + verdict (issue 2032). This is the thin I/O shell around
 * the pure, unit-tested modules (`proc` / `derive` / `csv` / `verdict` /
 * `report`): it spawns processes, moves files, and prints — all arithmetic and
 * formatting live in those modules.
 *
 * Local capture: fork `bash resource-sampler.sh` as a detached child that
 * watches the orchestrator PID (so it self-terminates if the parent crashes)
 * and writes a raw-counter CSV live (so the CSV survives a crash). At run end
 * the sampler is signalled, the raw CSV is derived into a Prometheus-overlay
 * CSV, and the summary + RESOURCE_STARVED verdict are printed and written.
 *
 * SSH-remote capture: the SAME shell script is piped over `ssh … bash -s` to
 * the remote box (the box whose CPU actually matters), mirroring how issue 2043
 * ships/executes remote bot commands. The remote CSV is `cat`-ed back to the
 * local run artifact dir at run end, then derived locally with the identical
 * code path.
 */

import { spawn as nodeSpawn, type ChildProcess } from "node:child_process";
import { createWriteStream, mkdirSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { taggedLine } from "../log-line";
import { reapChild, type ReapHandle } from "../control/reap-child";
import { buildBaseSshArgs, shellEscape, type SshHost } from "../control/ssh-hosts";
import type { ArrivalSpread } from "./arrival";
import { formatDerivedCsv } from "./csv";
import { deriveSamples } from "./derive";
import { type FpsStats } from "./fps";
import { parseRawCsv } from "./proc";
import { formatResourceReport, type ReportInput } from "./report";
import { evaluateVerdict, summarize, type ResourceSummary, type ResourceVerdict } from "./verdict";

/** Binds `taggedLine`'s label for this module's single `[resource]` marker. */
const resourceLine = (msg: string): string => taggedLine("resource", msg);

/** Absolute path to the shell sampler shipped alongside this package. */
export function resolveSamplerScriptPath(): string {
  // src/resource/session.ts → ../../scripts/resource-sampler.sh
  const here = dirname(fileURLToPath(import.meta.url));
  return join(here, "..", "..", "scripts", "resource-sampler.sh");
}

export interface ResourceCaptureOptions {
  /** Run artifact dir (the CLI's `--assets-dir`); CSVs land in `<runDir>/resource/`. */
  runDir: string;
  /** Sampling cadence in seconds. Default 5. */
  intervalSec?: number;
  /** Regex (bash ERE) matched against each process's /proc/PID/comm. Default: chrome/chromium/node/tsx. */
  procGrep?: string;
  /** Stable run label used in artifact filenames + the sampler meta row. */
  label?: string;
  /** Injected `spawn` for tests. Defaults to `child_process.spawn`. */
  spawn?: typeof nodeSpawn;
}

export interface ResourceCaptureResult {
  summary: ResourceSummary;
  verdict: ResourceVerdict;
  reportText: string;
  rawCsvPath: string;
  derivedCsvPath: string;
}

// Matched against /proc/PID/comm by the sampler (short names), so keep this
// comm-oriented — `chrome` covers browser + renderers, `node`/`tsx` the
// orchestrator/bot. The watched run PID is always included regardless.
const DEFAULT_PROC_GREP = "chrome|chromium|node|tsx";

/**
 * SSH keepalive options appended to every resource-sampler ssh invocation so a
 * dropped TRANSPORT cannot hang `finalizeAll` indefinitely: ssh probes every 5s
 * and gives up after 3 unanswered probes (~15s).
 *
 * Scope correction (issue 2133 sweep): this bounds the transport ONLY. A remote
 * box whose sshd keeps answering keepalives while `cat` never completes — a
 * wedged filesystem, a saturated box, a hung NFS read on the CSV path — leaves
 * the session alive and silent, and ServerAlive will never notice. That case is
 * bounded separately by {@link RETRIEVE_STALL_TIMEOUT_MS} in `retrieve`.
 */
export const SSH_KEEPALIVE_ARGS = ["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"];

/**
 * Inactivity (stall) bound for a remote CSV `retrieve`: the transfer is severed
 * only after this long with NO new stdout bytes. Reset on every chunk.
 *
 * Why INACTIVITY and not an absolute deadline — this is the opposite call from
 * the probe's {@link import("../control/ssh-hosts").SSH_PROBE_TIMEOUT_MS}, and
 * the difference is the point:
 *
 *   - The probe runs a FIXED, tiny command (`echo && uname -a`). Its total
 *     runtime has a known ceiling, so an absolute bound is both correct and
 *     simpler.
 *   - `retrieve` streams a file whose size scales with RUN LENGTH: one sampler
 *     row every `intervalSec` (default 5s), so a multi-hour run's raw CSV is
 *     orders of magnitude larger than a 10-minute run's. ANY absolute bound
 *     would either be uselessly loose for short runs or would sever a large
 *     transfer that is progressing perfectly well — losing the very artifact we
 *     are trying to save. Progress, not elapsed time, is the health signal
 *     here.
 *
 * 20s of total silence: comfortably above the ~15s ServerAlive teardown (so a
 * broken TRANSPORT is reported by ssh, with its better diagnostic, rather than
 * by this timer), and far above any plausible gap between TCP segments of a
 * healthy `cat` even on a slow WAN link. A transfer that is merely SLOW is
 * never severed — only one that has genuinely stopped.
 */
export const RETRIEVE_STALL_TIMEOUT_MS = 20_000;

/**
 * Grace between the retrieve's `SIGTERM` and the follow-up `SIGKILL`. Same
 * rationale as the probe's: an `ssh` blocked in a syscall can ignore SIGTERM,
 * and nothing waits on this window — `finalizeAll` already has its answer.
 */
export const RETRIEVE_KILL_GRACE_MS = 2_000;

/**
 * A resource capture bound to one run. Construct, `startLocal()` before the
 * bots launch, and `finalize()` after they finish. Every method is best-effort:
 * a spawn failure disables capture with a warning rather than failing the run —
 * resource capture is diagnostic, never load-bearing for the bots themselves.
 */
export class ResourceCaptureSession {
  readonly runDir: string;
  readonly intervalSec: number;
  readonly procGrep: string;
  readonly label: string;
  readonly rawCsvPath: string;
  readonly derivedCsvPath: string;
  readonly reportPath: string;

  private readonly spawn: typeof nodeSpawn;
  private child: ChildProcess | null = null;
  private startError: string | null = null;

  constructor(opts: ResourceCaptureOptions) {
    this.runDir = opts.runDir;
    this.intervalSec = opts.intervalSec ?? 5;
    this.procGrep = opts.procGrep ?? DEFAULT_PROC_GREP;
    this.label = opts.label ?? `run-${new Date().toISOString().replace(/[:.]/g, "-")}`;
    this.spawn = opts.spawn ?? nodeSpawn;
    const dir = join(this.runDir, "resource");
    this.rawCsvPath = join(dir, `${this.label}-raw.csv`);
    this.derivedCsvPath = join(dir, `${this.label}-derived.csv`);
    this.reportPath = join(dir, `${this.label}-summary.txt`);
  }

  /**
   * Fork the local sampler. Detached with its own process group so an
   * orchestrator crash leaves it running long enough to flush (its `--watch-pid`
   * then self-terminates it); `unref()` so it never keeps the Node event loop
   * alive on its own.
   */
  startLocal(): void {
    try {
      mkdirSync(join(this.runDir, "resource"), { recursive: true });
      const args = [
        resolveSamplerScriptPath(),
        "--out",
        this.rawCsvPath,
        "--interval",
        String(this.intervalSec),
        "--proc-grep",
        this.procGrep,
        "--watch-pid",
        String(process.pid),
        "--label",
        this.label,
      ];
      const child = this.spawn("bash", args, {
        detached: true,
        stdio: "ignore",
      });
      child.on("error", (e: Error) => {
        this.startError = e.message;
        console.warn(resourceLine(`sampler spawn error: ${e.message} — capture disabled`));
      });
      child.unref();
      this.child = child;
      console.log(
        resourceLine(
          `sampling host every ${this.intervalSec}s → ${this.rawCsvPath} (pid ${child.pid ?? "?"})`,
        ),
      );
    } catch (e) {
      this.startError = (e as Error).message;
      console.warn(
        resourceLine(`could not start sampler: ${(e as Error).message} — capture disabled`),
      );
    }
  }

  /**
   * Stop the sampler, derive the raw CSV, write the derived CSV + summary, and
   * print the report. Returns `null` when capture never produced a readable CSV
   * (spawn failed, or non-Linux box with no rows) — the caller treats that as
   * "no verdict available", not an error.
   */
  async finalize(
    fpsByBot: ReadonlyMap<string, FpsStats>,
    arrival: ArrivalSpread | null,
    joinedBots: number | null,
  ): Promise<ResourceCaptureResult | null> {
    await this.stopChild();
    let raw: string;
    try {
      raw = await readFile(this.rawCsvPath, "utf8");
    } catch {
      if (this.startError === null) {
        console.warn(resourceLine(`no raw CSV at ${this.rawCsvPath} — capture produced nothing`));
      }
      return null;
    }
    return deriveReport({
      rawCsvText: raw,
      rawCsvPath: this.rawCsvPath,
      derivedCsvPath: this.derivedCsvPath,
      reportPath: this.reportPath,
      fpsByBot,
      arrival,
      joinedBots,
    });
  }

  private async stopChild(): Promise<void> {
    const child = this.child;
    if (child === null || child.pid === undefined) return;
    try {
      // SIGTERM the sampler; it flushes the in-progress tick and exits 0.
      process.kill(child.pid, "SIGTERM");
    } catch {
      // Already gone — nothing to do.
    }
    await waitForExit(child, 3_000);
  }
}

/**
 * Read + derive a raw CSV (local or retrieved-from-remote) and render the
 * report. Split out from the class so both the local and SSH-remote paths run
 * the identical derive → summarize → verdict → report pipeline. Writes the
 * derived CSV + summary file next to the raw CSV and returns the result; the
 * caller prints `reportText`.
 */
export async function deriveReport(args: {
  rawCsvText: string;
  rawCsvPath: string;
  derivedCsvPath: string;
  reportPath: string;
  fpsByBot: ReadonlyMap<string, FpsStats>;
  arrival: ArrivalSpread | null;
  /** Bots seen to join; `null` when this process does not observe joins. */
  joinedBots: number | null;
}): Promise<ResourceCaptureResult> {
  const parsed = parseRawCsv(args.rawCsvText);
  const derived = deriveSamples(parsed);
  const summary = summarize(derived);
  const verdict = evaluateVerdict(derived, args.fpsByBot, args.joinedBots);
  const reportInput: ReportInput = {
    summary,
    verdict,
    fpsByBot: args.fpsByBot,
    arrival: args.arrival,
    supported: !parsed.unsupported,
    sysstatMissing: parsed.meta ? !(parsed.meta.haveMpstat && parsed.meta.havePidstat) : false,
    rawCsvPath: args.rawCsvPath,
    derivedCsvPath: args.derivedCsvPath,
  };
  const reportText = formatResourceReport(reportInput);
  await writeFile(args.derivedCsvPath, formatDerivedCsv(derived), "utf8").catch(() => {});
  await writeFile(args.reportPath, reportText + "\n", "utf8").catch(() => {});
  return {
    summary,
    verdict,
    reportText,
    rawCsvPath: args.rawCsvPath,
    derivedCsvPath: args.derivedCsvPath,
  };
}

// ── SSH-remote sampler ──────────────────────────────────────────────────────

export interface RemoteSamplerOptions {
  host: SshHost;
  intervalSec?: number;
  procGrep?: string;
  /** Hard upper bound (s) so a stranded remote sampler can never orphan. */
  maxSeconds: number;
  label?: string;
  /** Remote path the sampler writes its raw CSV to. Default under /tmp. */
  remoteCsvPath?: string;
  spawn?: typeof nodeSpawn;
  /**
   * Overrides {@link RETRIEVE_STALL_TIMEOUT_MS} for `retrieve`. Exists so tests
   * can bound a stalled transfer in milliseconds instead of waiting out the
   * production default; production callers omit it.
   */
  retrieveStallMs?: number;
  /** Overrides {@link RETRIEVE_KILL_GRACE_MS} for `retrieve`. */
  retrieveKillGraceMs?: number;
}

export interface RemoteSamplerHandle {
  host: SshHost;
  remoteCsvPath: string;
  /** Stop the remote sampler (kills the SSH session; the --max-seconds cap backstops). */
  stop(): void;
  /** `cat` the remote CSV back to `localPath`. Resolves to bytes written (0 on failure). */
  retrieve(localPath: string): Promise<number>;
}

/**
 * Launch the sampler ON a remote host by piping the shell script over
 * `ssh … bash -s -- <args>`. The SSH session stays in the foreground for the
 * sampler's lifetime (mirroring `spawnRemoteBot`); killing the local `ssh`
 * child ends the session and the `--max-seconds` cap guarantees termination
 * even if the signal does not propagate without a TTY.
 */
export function startRemoteSampler(
  scriptText: string,
  opts: RemoteSamplerOptions,
): RemoteSamplerHandle {
  const spawnImpl = opts.spawn ?? nodeSpawn;
  const interval = opts.intervalSec ?? 5;
  const grep = opts.procGrep ?? DEFAULT_PROC_GREP;
  const remoteCsvPath = opts.remoteCsvPath ?? `/tmp/bots-app-resource-${opts.host.label}.csv`;
  const label = opts.label ?? "remote";
  // `bash -s -- <args>` reads the script from stdin and passes the rest as
  // positional params; every dynamic value is single-quoted via shellEscape.
  const remoteCmd =
    `bash -s -- --out ${shellEscape(remoteCsvPath)} --interval ${shellEscape(String(interval))}` +
    ` --proc-grep ${shellEscape(grep)} --max-seconds ${shellEscape(String(opts.maxSeconds))}` +
    ` --label ${shellEscape(label)}`;
  const args = [
    ...buildBaseSshArgs(opts.host, { connectTimeout: 10 }),
    ...SSH_KEEPALIVE_ARGS,
    remoteCmd,
  ];
  const child = spawnImpl("ssh", args, { stdio: ["pipe", "ignore", "pipe"] });
  child.stdin?.write(scriptText);
  child.stdin?.end();
  child.stderr?.on("data", (b: Buffer) => {
    const msg = b.toString("utf8").trim();
    if (msg !== "") console.warn(resourceLine(`remote sampler (${opts.host.label}): ${msg}`));
  });

  return {
    host: opts.host,
    remoteCsvPath,
    stop(): void {
      try {
        child.kill("SIGTERM");
      } catch {
        // already gone
      }
    },
    async retrieve(localPath: string): Promise<number> {
      const catArgs = [
        ...buildBaseSshArgs(opts.host, { connectTimeout: 10 }),
        ...SSH_KEEPALIVE_ARGS,
        `cat ${shellEscape(remoteCsvPath)}`,
      ];
      const stallMs = opts.retrieveStallMs ?? RETRIEVE_STALL_TIMEOUT_MS;
      const killGraceMs = opts.retrieveKillGraceMs ?? RETRIEVE_KILL_GRACE_MS;
      // `reapPending` holds the in-flight escalation (if any) so we can await
      // it BEFORE this function returns. That await is load-bearing, not
      // hygiene: the caller chain (`finalizeAll` → `orchestrator.ts:596` →
      // `cli.ts:508`) hits `process.exit(0)` right after, and `process.exit`
      // does not run pending timers — so without awaiting, the SIGKILL is cut
      // off mid-flight and the wedged `ssh` is orphaned. The happy path never
      // pays for this: `reapPending` stays undefined unless we actually killed
      // something.
      let reapPending: Promise<void> | undefined;
      const bytes = await new Promise<number>((resolve) => {
        let child2: ChildProcess;
        try {
          child2 = spawnImpl("ssh", catArgs, { stdio: ["ignore", "pipe", "pipe"] });
        } catch (e) {
          console.warn(resourceLine(`remote CSV retrieve spawn failed: ${(e as Error).message}`));
          resolve(0);
          return;
        }
        const out = createWriteStream(localPath);
        let bytesSeen = 0;
        // DRAIN stderr. `stdio` pipes it, and an unread pipe has a ~64 KB kernel
        // buffer — once ssh fills it (host-key notices, banners, a verbose
        // warning per line on a misconfigured host) the child BLOCKS on write and
        // never finishes, so the transfer stalls out and the CSV is lost. That is
        // a deadlock this issue's own new stall bound would merely time out
        // rather than prevent. `startRemoteSampler` above already drains its
        // stderr for this reason; `retrieve` did not. Measured: drained -> 16692
        // bytes, exit 0, 64 ms; not drained -> 0 bytes and a stall timeout.
        // Captured (bounded) so a failure message can name the actual ssh error
        // instead of a bare exit code.
        let stderrTail = "";
        child2.stderr?.on("data", (b: Buffer) => {
          if (stderrTail.length < 4096) stderrTail += b.toString("utf8");
        });
        // Settle-once + clear-every-timer, same discipline as the probe
        // (`runSshProbe`): this promise has FIVE resolve paths (the spawn throw
        // above, `error`, the stream's `error`, `close`, and the stall timer) and
        // must resolve exactly once. A killed child still emits `close` after the
        // stall timer has resolved, and a dangling `setTimeout` would keep the
        // orchestrator's event loop alive at the very end of a run.
        let settled = false;
        let stallTimer: NodeJS.Timeout | undefined;
        let cancelReap: ReapHandle["cancel"] | undefined;
        // First GENUINE write failure seen on `out`, if any. Recorded because a
        // late ENOSPC can land after `pipe` already ended the stream, in which
        // case there is no `end()` callback left to carry it.
        let writeError: Error | null = null;
        const clearTimers = (): void => {
          if (stallTimer !== undefined) clearTimeout(stallTimer);
          if (cancelReap !== undefined) cancelReap();
          stallTimer = undefined;
          cancelReap = undefined;
        };
        /** Start the SIGTERM → grace → SIGKILL escalation and record it. */
        const startReap = (): void => {
          // `reapPending` is what makes the escalation survive this caller:
          // it exits the process immediately after awaiting (cli.ts:508), and
          // `process.exit` runs no pending timers — so the SIGKILL must be
          // awaited, not merely scheduled. See reapChild's note.
          const reap = reapChild(child2, { graceMs: killGraceMs });
          cancelReap = reap.cancel;
          reapPending = reap.settled;
        };
        /**
         * Close `out` and report a GENUINE write failure, if any.
         *
         * The subtlety that makes this its own function: `stdout.pipe(out)`
         * AUTO-ENDS `out` at source EOF. So on the NORMAL path — the child
         * streams the whole CSV and exits — the stream is already ended (usually
         * already finished) by the time we get here, and calling `end()` again
         * hands the callback `ERR_STREAM_ALREADY_FINISHED`. That code means
         * "already flushed successfully", NOT "flush failed": treating it as a
         * failure discarded a COMPLETE CSV as 0 bytes on every successful
         * transfer, which `finalizeAll` then dropped (`if (bytes === 0) continue`).
         * That is strictly worse than the late-error bug this logic exists for,
         * so the two cases are distinguished explicitly rather than by
         * "truthy err".
         *
         * States, and what each needs:
         *   - already FINISHED  → nothing to do; report any recorded error.
         *   - already ENDED     → `pipe` started the flush; wait for `finish`.
         *   - not ended (stall) → we must `end()` it ourselves: that is what
         *     flushes buffered bytes and releases the fd, so skipping it leaks a
         *     descriptor per wedged host.
         */
        const closeOut = (done: (err: Error | null) => void): void => {
          let called = false;
          const finish = (err: Error | null): void => {
            if (called) return;
            called = true;
            done(err ?? writeError);
          };
          // A late failure surfacing during the flush must win over a clean
          // finish, so listen for it regardless of which branch we take below.
          out.once("error", finish);
          if (out.writableFinished) {
            finish(null);
            return;
          }
          if (out.writableEnded) {
            out.once("finish", () => finish(null));
            return;
          }
          out.end((err?: Error | null) => finish(benignCloseError(err) ? null : (err ?? null)));
        };
        /**
         * Resolve once, always closing the write stream first.
         *
         * A genuine flush failure downgrades the result to 0 ("no usable CSV"):
         * the flush is where a late ENOSPC/EDQUOT surfaces, AFTER `exit` has
         * already reported success, and reporting the byte count there would
         * hand `finalizeAll` a truncated CSV it believes is complete.
         */
        const settle = (value: number): void => {
          if (settled) return;
          settled = true;
          clearTimers();
          closeOut((err) => {
            if (err) {
              console.warn(
                resourceLine(
                  `remote CSV write to ${localPath} failed while flushing: ` +
                    `${err.message} — reporting 0 bytes rather than a truncated CSV`,
                ),
              );
              resolve(0);
              return;
            }
            resolve(value);
          });
        };
        // INACTIVITY bound, not absolute: rearmed on every chunk, so a large
        // but progressing transfer is never severed (see
        // RETRIEVE_STALL_TIMEOUT_MS for why this site differs from the probe).
        const armStall = (): void => {
          if (settled) return;
          if (stallTimer !== undefined) clearTimeout(stallTimer);
          stallTimer = setTimeout(() => {
            console.warn(
              resourceLine(
                `remote CSV retrieve from ${opts.host.label} stalled — no data for ` +
                  `${stallMs}ms (${bytesSeen} bytes received); killing the ssh session. The transport ` +
                  `was alive (ServerAlive kept answering), so the remote \`cat\` itself wedged.`,
              ),
            );
            // Resolve FIRST, then reap: `settle` runs `clearTimers`, so
            // arming the reaper before it would cancel the escalation.
            settle(0);
            startReap();
          }, stallMs);
        };
        child2.stdout?.on("data", (b: Buffer) => {
          bytesSeen += b.length;
          armStall();
        });
        child2.stdout?.pipe(out);
        // A WriteStream `error` (unwritable path, disk full, EDQUOT) is emitted
        // ASYNCHRONOUSLY, so it does NOT propagate to `finalizeAll`'s
        // try/catch — with no listener it becomes an uncaughtException and
        // takes down the orchestrator at run end. That is unacceptable on a
        // path documented as best-effort, so handle it like every other
        // failure: warn and report 0 bytes.
        //
        // A LATE error (after `exit` already settled) is carried by `writeError`
        // / the `closeOut` listener instead — by then this listener's `settled`
        // guard has already fired.
        out.on("error", (e: Error) => {
          if (writeError === null) writeError = e;
          console.warn(resourceLine(`remote CSV write to ${localPath} failed: ${e.message}`));
          if (settled) return;
          settled = true;
          clearTimers();
          // Do NOT call `out.end()` on an errored stream; destroy it instead.
          out.destroy();
          resolve(0);
          startReap();
        });
        child2.on("error", (e: Error) => {
          console.warn(resourceLine(`remote CSV retrieve failed: ${e.message}`));
          settle(0);
        });
        // `close`, NOT `exit` — this is the ROOT-CAUSE fix for a whole class of
        // silent CSV truncation, and the distinction is the entire bug:
        //
        //   * `exit`  fires when the child PROCESS dies. Its stdio may still be
        //             draining.
        //   * `close` fires when the process has died AND every stdio stream has
        //             been fully consumed.
        //
        // Settling on `exit` meant `settle()` could run mid-pipe, so `closeOut`
        // saw `writableEnded === false` and called `out.end()` on a stream the
        // pipe was still writing to — TRUNCATING the CSV. The pipe's next write
        // then raised `ERR_STREAM_WRITE_AFTER_END` into the pending `end`
        // callback, which a benign-code whitelist happily reported as success.
        // Net: a short file returned as complete, and `finalizeAll` deriving a
        // verdict from partial data. Measured on a slow sink (which is what a
        // real disk/NFS write is): at `exit`, 1,785,856 of 2,000,000 bytes were
        // written, `writableEnded=false`. At `close`: all 2,000,000,
        // `writableEnded=true`.
        //
        // On `close` the pipe has ALWAYS auto-ended the stream, so `closeOut`
        // can only ever take its already-ended branch and the truncating path is
        // now unreachable rather than merely guarded. That is why this is the
        // correct fix and a wider whitelist was not: the whitelist suppressed the
        // SYMPTOM of writing to a stream we should not have ended yet.
        child2.on("close", (code) => {
          // Cancel the reaper before the settled-check: a child exiting in
          // response to our own SIGTERM arrives when `settled` is already
          // true, and leaving the grace timer armed would fire SIGKILL at a
          // pid the OS may have recycled.
          clearTimers();
          if (settled) return;
          if (code !== 0) {
            // Name the actual ssh failure rather than a bare exit code — this is
            // why stderr is captured above and not just drained.
            console.warn(
              resourceLine(
                `remote CSV retrieve from ${opts.host.label} exited ${code}` +
                  (stderrTail.trim() ? `: ${stderrTail.trim()}` : ""),
              ),
            );
          }
          settle(code === 0 ? bytesSeen : 0);
        });
        armStall();
      });
      // Let a started escalation run to completion before returning, so the
      // caller's `process.exit` cannot cut the SIGKILL off. Bounded by
      // `killGraceMs` (2s default) and only ever reached on a failure path.
      if (reapPending !== undefined) await reapPending;
      return bytes;
    },
  };
}

/**
 * Manages one remote sampler per distinct SSH host used during a run (issue
 * 2032). The orchestrator's SSH launch path calls {@link ensureForHost} for
 * each bot; the manager starts a sampler the FIRST time it sees a host label
 * (so N bots on one box share one sampler, not N) and {@link finalizeAll}
 * retrieves + derives every host's CSV at run end. Fully best-effort and
 * guarded so it can never affect the bots' own lifecycle.
 */
export class RemoteResourceManager {
  private readonly byHost = new Map<string, RemoteSamplerHandle>();
  private scriptText: string | null = null;

  constructor(
    private readonly opts: {
      runDir: string;
      label: string;
      intervalSec?: number;
      procGrep?: string;
      /** Hard cap for every remote sampler (orphan safety). */
      maxSeconds: number;
      spawn?: typeof nodeSpawn;
      /** Injected script text for tests; read from disk when omitted. */
      scriptText?: string;
      /**
       * Overrides {@link RETRIEVE_STALL_TIMEOUT_MS} for every host's `retrieve`.
       * Threaded through to {@link startRemoteSampler} so a test can exercise
       * the bound via `finalizeAll` — the real call site — rather than only via
       * a hand-built handle. Production omits it and gets the default.
       */
      retrieveStallMs?: number;
      /** Overrides {@link RETRIEVE_KILL_GRACE_MS}. */
      retrieveKillGraceMs?: number;
    },
  ) {}

  /**
   * Start a sampler on `host` if one is not already running there. Never
   * throws — a failure disables remote capture for that host with a warning.
   */
  async ensureForHost(host: SshHost): Promise<void> {
    if (this.byHost.has(host.label)) return;
    // Reserve the slot synchronously so two near-simultaneous launches on the
    // same host do not both start a sampler.
    this.byHost.set(host.label, PLACEHOLDER_HANDLE);
    try {
      if (this.scriptText === null) {
        this.scriptText = this.opts.scriptText ?? (await readSamplerScript());
      }
      if (this.scriptText === "") {
        this.byHost.delete(host.label);
        return;
      }
      const handle = startRemoteSampler(this.scriptText, {
        host,
        intervalSec: this.opts.intervalSec,
        procGrep: this.opts.procGrep,
        maxSeconds: this.opts.maxSeconds,
        label: this.opts.label,
        spawn: this.opts.spawn,
        retrieveStallMs: this.opts.retrieveStallMs,
        retrieveKillGraceMs: this.opts.retrieveKillGraceMs,
      });
      this.byHost.set(host.label, handle);
      console.log(resourceLine(`remote sampling ${host.label} (${host.user}@${host.host})`));
    } catch (e) {
      this.byHost.delete(host.label);
      console.warn(
        resourceLine(`remote sampler for ${host.label} failed: ${(e as Error).message}`),
      );
    }
  }

  /**
   * Stop, retrieve, and derive every remote host's CSV. Returns one result per
   * host that produced a readable CSV; prints each report. Remote bots run
   * `launchBot` off-process so no per-bot FPS is available — the remote verdict
   * is CPU-only (pass an empty fps map).
   */
  async finalizeAll(): Promise<ResourceCaptureResult[]> {
    const results: ResourceCaptureResult[] = [];
    for (const [hostLabel, handle] of this.byHost) {
      if (handle === PLACEHOLDER_HANDLE) continue;
      handle.stop();
      const rawCsvPath = join(
        this.opts.runDir,
        "resource",
        `${this.opts.label}-${hostLabel}-raw.csv`,
      );
      try {
        mkdirSync(join(this.opts.runDir, "resource"), { recursive: true });
        const bytes = await handle.retrieve(rawCsvPath);
        if (bytes === 0) {
          console.warn(resourceLine(`no remote CSV retrieved from ${hostLabel}`));
          continue;
        }
        const rawText = await readFile(rawCsvPath, "utf8");
        const result = await deriveReport({
          rawCsvText: rawText,
          rawCsvPath,
          derivedCsvPath: join(
            this.opts.runDir,
            "resource",
            `${this.opts.label}-${hostLabel}-derived.csv`,
          ),
          reportPath: join(
            this.opts.runDir,
            "resource",
            `${this.opts.label}-${hostLabel}-summary.txt`,
          ),
          fpsByBot: new Map(),
          // A remote box's bots join out of this process, like its fps.
          arrival: null,
          joinedBots: null,
        });
        console.log(resourceLine(`remote host ${hostLabel}:`));
        console.log(result.reportText);
        results.push(result);
      } catch (e) {
        console.warn(
          resourceLine(`remote finalize for ${hostLabel} failed: ${(e as Error).message}`),
        );
      }
    }
    return results;
  }
}

/**
 * Codes that a redundant `end()` reports when the stream was ALREADY closed
 * cleanly. They mean "already flushed successfully", not "the flush failed".
 *
 * `stdout.pipe(out)` auto-ends `out` at source EOF, so on every SUCCESSFUL
 * transfer the stream is already ended/finished before `settle` runs — making
 * these the NORMAL outcome of the belt-and-braces `end()`, not an edge case.
 * Treating them as failures reported a complete CSV as 0 bytes, and
 * `finalizeAll` dropped the host (`if (bytes === 0) continue`).
 *
 * Verified against Node 22:
 *   - finished, then `end()` again        → ERR_STREAM_ALREADY_FINISHED
 *   - ended-not-yet-finished, `end()`     → no error
 *   - `write()` after `end()`             → ERR_STREAM_WRITE_AFTER_END
 *
 * A genuine ENOSPC/EDQUOT carries neither code and still downgrades to 0.
 *
 * ONLY `ERR_STREAM_ALREADY_FINISHED` is listed, and `ERR_STREAM_WRITE_AFTER_END`
 * is DELIBERATELY EXCLUDED — adding it hid a real defect. It is a WRITE-path
 * error, so it can only surface on a write callback or the `error` event, never
 * in a first-`end()` callback (the sole place this predicate is consulted). It
 * therefore cannot describe a redundant close at all; the only thing it could
 * ever do here is mask a TRUNCATED file as success — which is exactly what it
 * did when `retrieve` still settled on `exit` and could `end()` mid-pipe. That
 * root cause is fixed (settle on `close`), so this predicate is now only ever
 * reached with the stream genuinely finished. Do not re-add it: silently
 * reporting a short CSV as complete is worse than reporting 0.
 */
const BENIGN_CLOSE_CODES = new Set(["ERR_STREAM_ALREADY_FINISHED"]);

/** True when `err` only says the stream was already closed cleanly. */
function benignCloseError(err: unknown): boolean {
  if (err === null || err === undefined) return true;
  const code = (err as NodeJS.ErrnoException).code;
  return code !== undefined && BENIGN_CLOSE_CODES.has(code);
}

/** Sentinel occupying a host slot between reservation and a started handle. */
const PLACEHOLDER_HANDLE = {} as RemoteSamplerHandle;

/**
 * Read the shell sampler script text (for piping to a remote host). Cached
 * read; falls back to an empty string with a warning if the script is missing,
 * which disables remote capture without failing the run.
 */
export async function readSamplerScript(): Promise<string> {
  try {
    return await readFile(resolveSamplerScriptPath(), "utf8");
  } catch (e) {
    console.warn(resourceLine(`could not read sampler script: ${(e as Error).message}`));
    return "";
  }
}

function waitForExit(child: ChildProcess, timeoutMs: number): Promise<void> {
  return new Promise<void>((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve();
      return;
    }
    let done = false;
    const finish = (): void => {
      if (done) return;
      done = true;
      resolve();
    };
    child.once("exit", finish);
    setTimeout(finish, timeoutMs).unref?.();
  });
}
