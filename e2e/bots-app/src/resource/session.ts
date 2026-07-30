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

import { buildBaseSshArgs, shellEscape, type SshHost } from "../control/ssh-hosts";
import { formatDerivedCsv } from "./csv";
import { deriveSamples } from "./derive";
import { type FpsStats } from "./fps";
import { parseRawCsv } from "./proc";
import { formatResourceReport, type ReportInput } from "./report";
import { evaluateVerdict, summarize, type ResourceSummary, type ResourceVerdict } from "./verdict";

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
 * mid-transfer stall (a wedged remote box, a silently dropped TCP connection)
 * cannot hang `finalizeAll` indefinitely: ssh probes every 5s and gives up
 * after 3 unanswered probes (~15s).
 */
const SSH_KEEPALIVE_ARGS = ["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"];

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
        console.warn(`[resource] sampler spawn error: ${e.message} — capture disabled`);
      });
      child.unref();
      this.child = child;
      console.log(
        `[resource] sampling host every ${this.intervalSec}s → ${this.rawCsvPath} (pid ${child.pid ?? "?"})`,
      );
    } catch (e) {
      this.startError = (e as Error).message;
      console.warn(
        `[resource] could not start sampler: ${(e as Error).message} — capture disabled`,
      );
    }
  }

  /**
   * Stop the sampler, derive the raw CSV, write the derived CSV + summary, and
   * print the report. Returns `null` when capture never produced a readable CSV
   * (spawn failed, or non-Linux box with no rows) — the caller treats that as
   * "no verdict available", not an error.
   */
  async finalize(fpsByBot: ReadonlyMap<string, FpsStats>): Promise<ResourceCaptureResult | null> {
    await this.stopChild();
    let raw: string;
    try {
      raw = await readFile(this.rawCsvPath, "utf8");
    } catch {
      if (this.startError === null) {
        console.warn(`[resource] no raw CSV at ${this.rawCsvPath} — capture produced nothing`);
      }
      return null;
    }
    return deriveReport({
      rawCsvText: raw,
      rawCsvPath: this.rawCsvPath,
      derivedCsvPath: this.derivedCsvPath,
      reportPath: this.reportPath,
      fpsByBot,
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
}): Promise<ResourceCaptureResult> {
  const parsed = parseRawCsv(args.rawCsvText);
  const derived = deriveSamples(parsed);
  const summary = summarize(derived);
  const verdict = evaluateVerdict(derived, args.fpsByBot);
  const reportInput: ReportInput = {
    summary,
    verdict,
    fpsByBot: args.fpsByBot,
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
    if (msg !== "") console.warn(`[resource] remote sampler (${opts.host.label}): ${msg}`);
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
      return await new Promise<number>((resolve) => {
        let child2: ChildProcess;
        try {
          child2 = spawnImpl("ssh", catArgs, { stdio: ["ignore", "pipe", "pipe"] });
        } catch (e) {
          console.warn(`[resource] remote CSV retrieve spawn failed: ${(e as Error).message}`);
          resolve(0);
          return;
        }
        const out = createWriteStream(localPath);
        let bytes = 0;
        child2.stdout?.on("data", (b: Buffer) => {
          bytes += b.length;
        });
        child2.stdout?.pipe(out);
        child2.on("error", (e: Error) => {
          console.warn(`[resource] remote CSV retrieve failed: ${e.message}`);
          resolve(0);
        });
        child2.on("exit", (code) => {
          out.end(() => resolve(code === 0 ? bytes : 0));
        });
      });
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
      });
      this.byHost.set(host.label, handle);
      console.log(`[resource] remote sampling ${host.label} (${host.user}@${host.host})`);
    } catch (e) {
      this.byHost.delete(host.label);
      console.warn(`[resource] remote sampler for ${host.label} failed: ${(e as Error).message}`);
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
          console.warn(`[resource] no remote CSV retrieved from ${hostLabel}`);
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
        });
        console.log(`[resource] remote host ${hostLabel}:`);
        console.log(result.reportText);
        results.push(result);
      } catch (e) {
        console.warn(`[resource] remote finalize for ${hostLabel} failed: ${(e as Error).message}`);
      }
    }
    return results;
  }
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
    console.warn(`[resource] could not read sampler script: ${(e as Error).message}`);
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
