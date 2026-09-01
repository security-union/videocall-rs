import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { EventEmitter } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";

import {
  CAMERA_CYCLE_ENV_NAMES,
  CAMERA_CYCLE_SECS_CEILING,
  resolveCameraCycle,
  targetDutyPct,
} from "./camera-cycle";
import {
  IFACE_PATTERN,
  NETEM_BENIGN_CLEAR_ERRORS,
  NETEM_IFACE_DEFAULT,
  NETEM_IFB_DEV,
  NETEM_IFB_TXQUEUELEN,
  NETEM_INGRESS_QDISC_MARKER,
  NETEM_MIRROR_ADD_STEP,
  NETEM_PROFILES,
  NETEM_PROFILE_NAMES,
  buildNetemClearArgs,
  buildNetemMirrorClearArgs,
  buildNetemMirrorInstallArgs,
  buildNetemProbeArgs,
  buildNetemShapeArgs,
  ingressNetemParams,
  type NetemCommand,
} from "./control/netem";
import { ResourceCaptureSession } from "./resource/session";

// The PRODUCTION entrypoint — not a reimplementation. Reverting the ordinal
// resolution in this script makes the assertions below fail.
const ENTRYPOINT = fileURLToPath(new URL("../docker-entrypoint.sh", import.meta.url));
/** What real iproute2 prints for an interface nothing has shaped. */
const UNSHAPED_QDISC = "qdisc noqueue 0: root refcnt 2";

interface RunOpts {
  /** Stub `tc`/`ip` fails when its argv contains this substring. */
  failWhen?: string;
  /** What that failing command writes to stderr. */
  failStderr?: string;
  /** Exit status for that failure. 126/127 are the shell's "never ran" codes. */
  failStatus?: number;
  /** Forces `qdisc show dev <BOT_NETEM_IFACE>` output instead of the fake's state. */
  qdiscShow?: string;
  /** Forces `qdisc show dev ifb0` output. */
  ifbQdiscShow?: string;
  /** Seeds an `ifb0` the pod netns kept across a container restart. */
  preexistingIfb?: boolean;
  /** Seeds an ingress hook on `BOT_NETEM_IFACE`, as a restart would leave. */
  preexistingHook?: boolean;
}

/**
 * Stub `tc`/`ip` that MODELS the kernel rather than always succeeding: exit
 * statuses and messages are the ones real iproute2 produced for these commands
 * (`ip link add` over an existing device, `tc qdisc add … ingress` over an
 * existing hook, a delete of an absent object). A stub that returned 0 for
 * everything would pass a script whose post-read can never fail.
 */
function netStub(tool: "tc" | "ip", netLog: string, state: string, opts: RunOpts): string {
  const fail = opts.failWhen
    ? [
        `if [[ "${tool} $*" == *${JSON.stringify(opts.failWhen)}* ]]; then`,
        `  printf '%s\\n' ${JSON.stringify(opts.failStderr ?? "stub failure")} >&2`,
        `  exit ${opts.failStatus ?? 1}`,
        "fi",
      ].join("\n")
    : "true";
  // `%b`, so a multi-line fixture reaches the script as real newlines rather
  // than the two characters JSON.stringify escapes them to.
  const show = (fixture: string | undefined): string =>
    fixture === undefined ? ":" : `printf '%b\\n' ${JSON.stringify(fixture)}; exit 0`;
  const body =
    tool === "tc"
      ? `
case "$1 $2" in
  "qdisc show")
    dev="$4"
    if [ "$dev" = ifb0 ]; then
      ${show(opts.ifbQdiscShow)}
      [ -e "$S/dev-ifb0" ] || { printf 'Cannot find device "ifb0"\\n' >&2; exit 1; }
    else
      ${show(opts.qdiscShow)}
    fi
    if [ -e "$S/root-$dev" ]; then cat "$S/root-$dev"; else printf '%s\\n' ${JSON.stringify(UNSHAPED_QDISC)}; fi
    [ -e "$S/ingress-$dev" ] && printf '%s\\n' "qdisc ingress ffff: parent ffff:fff1 ----------------"
    exit 0
    ;;
  "qdisc replace")
    dev="$4"
    if [ "$5" = root ]; then
      shift 6
      printf '%s\\n' "qdisc netem 8001: root refcnt 2 $*" >"$S/root-$dev"
    fi
    exit 0
    ;;
  "qdisc del")
    dev="$4"
    if [ "$5" = root ]; then
      [ -e "$S/root-$dev" ] || { printf 'Error: Cannot delete qdisc with handle of zero.\\n' >&2; exit 2; }
      rm -f "$S/root-$dev"
    else
      [ -e "$S/ingress-$dev" ] || { printf 'Error: Invalid handle.\\n' >&2; exit 2; }
      rm -f "$S/ingress-$dev"
    fi
    exit 0
    ;;
  "qdisc add")
    dev="$4"
    [ -e "$S/ingress-$dev" ] && { printf 'Error: Exclusivity flag on, cannot modify.\\n' >&2; exit 2; }
    : >"$S/ingress-$dev"
    exit 0
    ;;
  "filter add")
    dev="$4"
    [ -e "$S/ingress-$dev" ] || { printf 'Error: Parent Qdisc doesn'"'"'t exists.\\n' >&2; exit 2; }
    exit 0
    ;;
esac
exit 0
`
      : `
case "$1 $2" in
  "link show")
    [ -e "$S/dev-$3" ] || { printf 'Device "%s" does not exist.\\n' "$3" >&2; exit 1; }
    exit 0
    ;;
  "link add")
    [ -e "$S/dev-$3" ] && { printf 'RTNETLINK answers: File exists\\n' >&2; exit 2; }
    : >"$S/dev-$3"
    exit 0
    ;;
  "link del")
    [ -e "$S/dev-$3" ] || { printf 'Cannot find device "%s"\\n' "$3" >&2; exit 1; }
    rm -f "$S/dev-$3" "$S/root-$3" "$S/ingress-$3"
    exit 0
    ;;
esac
exit 0
`;
  return [
    "#!/usr/bin/env bash",
    `S=${JSON.stringify(state)}`,
    `printf '%s\\n' "${tool} $*" >>${JSON.stringify(netLog)}`,
    fail,
    body,
    "",
  ].join("\n");
}

/**
 * Run the entrypoint with a controlled env in a hermetic workdir. A stub `tsx`
 * stands in for `node_modules/.bin/tsx` (the entrypoint `exec`s it as the final
 * step) and prints the credentials it inherited + the args it was handed, so the
 * test can assert what the entrypoint resolved WITHOUT launching a browser.
 * `tc`/`ip`/`sleep` are stubbed on PATH, so shaping is recorded in `commands`
 * rather than applied to the machine running this suite.
 */
function runEntrypoint(
  vars: Record<string, string>,
  opts: RunOpts = {},
): {
  status: number;
  stdout: string;
  stderr: string;
  commands: string[];
  /** Exact argv the entrypoint `exec`ed with; empty when it never reached `exec`. */
  argv: string[];
} {
  const workdir = mkdtempSync(join(tmpdir(), "bot-entrypoint-"));
  const binDir = join(workdir, "node_modules", ".bin");
  mkdirSync(binDir, { recursive: true });
  const netBin = join(workdir, "netbin");
  const state = join(workdir, "netstate");
  mkdirSync(netBin, { recursive: true });
  mkdirSync(state, { recursive: true });
  if (opts.preexistingIfb) writeFileSync(join(state, "dev-ifb0"), "");
  if (opts.preexistingHook)
    writeFileSync(join(state, `ingress-${vars.BOT_NETEM_IFACE ?? "eth0"}`), "");
  const netLog = join(workdir, "net-commands.log");
  for (const tool of ["tc", "ip"] as const) {
    const path = join(netBin, tool);
    writeFileSync(path, netStub(tool, netLog, state, opts));
    chmodSync(path, 0o755);
  }
  // Refuses unless net_admin was raised, so every mirror test locks both flags.
  {
    const path = join(netBin, "netem-setpriv");
    writeFileSync(
      path,
      [
        "#!/usr/bin/env bash",
        "inh=0; amb=0",
        "while [ $# -gt 0 ]; do",
        '  case "$1" in',
        '    --inh-caps) [ "$2" = "+net_admin" ] && inh=1; shift 2 ;;',
        '    --ambient-caps) [ "$2" = "+net_admin" ] && amb=1; shift 2 ;;',
        "    --) shift; break ;;",
        "    *) shift ;;",
        "  esac",
        "done",
        '[ "$inh" = 1 ] && [ "$amb" = 1 ] || {',
        '  printf "%s\\n" "netem-setpriv: net_admin not raised" >&2; exit 1;',
        "}",
        'exec "$@"',
        "",
      ].join("\n"),
    );
    chmodSync(path, 0o755);
  }
  {
    const path = join(netBin, "sleep");
    writeFileSync(
      path,
      [
        "#!/usr/bin/env bash",
        `printf '%s\\n' "sleep $*" >>${JSON.stringify(netLog)}`,
        opts.failWhen
          ? [
              `if [[ "sleep $*" == *${JSON.stringify(opts.failWhen)}* ]]; then`,
              `  printf '%s\\n' ${JSON.stringify(opts.failStderr ?? "stub failure")} >&2`,
              `  exit ${opts.failStatus ?? 1}`,
              "fi",
            ].join("\n")
          : "true",
        "exit 0",
        "",
      ].join("\n"),
    );
    chmodSync(path, 0o755);
  }
  const argvLog = join(workdir, "argv.bin");
  const stub = join(binDir, "tsx");
  // The entrypoint runs `exec node_modules/.bin/tsx …`, so this stub becomes the
  // process and inherits the (exported) BOT_EMAIL/BOT_PASSWORD + receives the
  // resolved `--participant` in its args.
  writeFileSync(
    stub,
    [
      "#!/usr/bin/env bash",
      'echo "STUB_EMAIL=${BOT_EMAIL-<unset>}"',
      'echo "STUB_PASSWORD=${BOT_PASSWORD-<unset>}"',
      // The camera cycle is consumed from the INHERITED env by cli.ts, not from
      // argv, so what the bot process sees is the contract (#2362).
      'echo "STUB_CAMERA=${BOT_CAMERA_ON_SECS_MIN-<unset>}|${BOT_CAMERA_ON_SECS_MAX-<unset>}|${BOT_CAMERA_OFF_SECS_MIN-<unset>}|${BOT_CAMERA_OFF_SECS_MAX-<unset>}"',
      // Argv reaches the bot verbatim by design; collapsed here so the stub's own
      // echo is not mistaken for the entrypoint's. The side file keeps exact bytes.
      `printf '%s\\0' "$@" >${JSON.stringify(argvLog)}`,
      'args="$*"',
      "echo \"STUB_ARGS=${args//[$'\\r\\n']/ }\"",
      "",
    ].join("\n"),
  );
  chmodSync(stub, 0o755);

  // Replace the env entirely (`env` does not merge) but keep PATH so
  // bash/mkdir/coreutils resolve. BOT_RUN_DIR is pointed inside the workdir so
  // the run is hermetic.
  const env: Record<string, string> = {
    PATH: `${netBin}:${process.env.PATH ?? "/usr/bin:/bin"}`,
    HOME: workdir,
    BOT_RUN_DIR: join(workdir, "run"),
    // An EMPTY value instead selects the production default: an in-image path.
    NETEM_SETPRIV: join(netBin, "netem-setpriv"),
    ...vars,
  };
  const recorded = (): string[] => {
    try {
      return readFileSync(netLog, "utf8").trim().split("\n").filter(Boolean);
    } catch {
      return [];
    }
  };
  const recordedArgv = (): string[] => {
    try {
      return readFileSync(argvLog, "utf8").split("\0").slice(0, -1);
    } catch {
      return [];
    }
  };
  try {
    const res = spawnSync("bash", [ENTRYPOINT], { cwd: workdir, env, encoding: "utf8" });
    return {
      status: res.status ?? 1,
      stdout: res.stdout ?? "",
      stderr: res.stderr ?? "",
      commands: recorded(),
      argv: recordedArgv(),
    };
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
}

function flagValue(argv: readonly string[], flag: string): string | undefined {
  const i = argv.indexOf(flag);
  return i === -1 ? undefined : argv[i + 1];
}

describe("docker-entrypoint.sh — ordinal identity resolution (#2035)", () => {
  // Guards the fleet's core mechanism: hostname videocall-bots-<N> → this pod's
  // BOT_EMAIL_<N>/BOT_PASSWORD_<N> selected from the whole-Secret env + exported,
  // handle = bot-<N>. A regression here would crash-loop every affected pod.
  const ORDINAL_ACCOUNTS = {
    BOT_EMAIL_0: "alice@example.test",
    BOT_PASSWORD_0: "pw-alice",
    BOT_EMAIL_1: "bob@example.test",
    BOT_PASSWORD_1: "pw-bob",
    BOT_EMAIL_2: "carol@example.test",
    BOT_PASSWORD_2: "pw-carol",
  };

  it("selects THIS ordinal's account from the fleet-wide env and derives bot-<N>", () => {
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-2",
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(0);
    // Picks ordinal 2's pair — NOT ordinal 0's (proves the indirect ${!var}
    // selection, not a fixed index) — and exports it (empty => export missing).
    expect(stdout).toContain("STUB_EMAIL=carol@example.test");
    expect(stdout).toContain("STUB_PASSWORD=pw-carol");
    // Handle is derived from the ordinal, not the shared template default.
    expect(stdout).toContain("--participant bot-2");
    expect(stdout).toContain("--display-name bot-2");
  });

  it("fails fast (exit 1) when the derived ordinal has no provisioned account", () => {
    const { status, stderr } = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-5", // only ordinals 0..2 are provisioned
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(1);
    expect(stderr).toContain("ordinal 5 has no provisioned account");
  });

  it("fails fast (exit 1) when the hostname yields a non-numeric ordinal", () => {
    const { status, stderr } = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-notanumber",
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(1);
    expect(stderr).toContain("could not derive a numeric ordinal");
  });

  it("single mode: uses BOT_EMAIL directly and leaves the ordinal path untouched", () => {
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "single",
      BOT_AUTH: "form-login",
      BOT_PARTICIPANT: "k8s-bot-1",
      BOT_EMAIL: "solo@example.test",
      BOT_PASSWORD: "pw-solo",
      HOSTNAME: "videocall-bots-9", // present but must be IGNORED in single mode
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(0);
    expect(stdout).toContain("STUB_EMAIL=solo@example.test");
    expect(stdout).toContain("--participant k8s-bot-1");
    // Must NOT have re-derived a bot-<N> handle from the hostname.
    expect(stdout).not.toContain("--participant bot-9");
  });

  it("defaults the simulcast cap to 3 rungs, matching what the k8s manifests declare", () => {
    // UNSET is the case the manifests rely on if the env is ever dropped, and it was
    // unpinned: the default could drift from statefulset.yaml/bot-pod.yaml silently.
    // 10 is the client's >=10 -> 3-layer threshold. Three rungs is deliberate — the cap
    // governs BOTH directions, and a 2-rung [low, hd] ladder leaves the #1256 tile lid
    // no middle rung, so a healthy mid-size grid decodes hd per peer (#2248).
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-0",
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(0);
    // Parsed, not substring-matched, so a drift to 100 cannot pass.
    const args = /^STUB_ARGS=(.*)$/m.exec(stdout)?.[1].split(/\s+/) ?? [];
    const flagIdx = args.indexOf("--hardware-concurrency");
    expect(flagIdx, "the entrypoint must pass the flag").toBeGreaterThan(-1);
    expect(args[flagIdx + 1]).toBe("10");

    // And the manifests must agree with that default, or a pod gets a different
    // ladder depth than a bare container and the fleet measurement is not reproducible.
    for (const manifest of ["k8s/statefulset.yaml", "k8s/bot-pod.yaml"]) {
      const yaml = readFileSync(fileURLToPath(new URL(`../${manifest}`, import.meta.url)), "utf8");
      const declared = /name:\s*BOT_HW_CONCURRENCY\s*\n\s*value:\s*"(\d+)"/.exec(yaml);
      expect(declared, `${manifest} must declare BOT_HW_CONCURRENCY`).not.toBeNull();
      expect(declared![1], `${manifest} must match the entrypoint default`).toBe("10");
    }
  });

  it("passes the pod's ordinal as --bot-index, beating any template BOT_INDEX (#2236)", () => {
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-2",
      BOT_INDEX: "0", // a shared template value must LOSE to the pod's hostname
      ...ORDINAL_ACCOUNTS,
    });
    expect(status).toBe(0);
    const args = /^STUB_ARGS=(.*)$/m.exec(stdout)?.[1].split(/\s+/) ?? [];
    expect(args[args.indexOf("--bot-index") + 1]).toBe("2");
  });

  it("passes a single-mode BOT_INDEX through — the bot-pod.yaml path (#2236)", () => {
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "single",
      BOT_AUTH: "form-login",
      BOT_PARTICIPANT: "k8s-bot-1",
      BOT_EMAIL: "solo@example.test",
      BOT_PASSWORD: "pw-solo",
      BOT_INDEX: "3",
    });
    expect(status).toBe(0);
    const args = /^STUB_ARGS=(.*)$/m.exec(stdout)?.[1].split(/\s+/) ?? [];
    expect(args[args.indexOf("--bot-index") + 1]).toBe("3");
  });

  it("omits --bot-index in single mode with BOT_INDEX unset (#2236)", () => {
    const { status, stdout } = runEntrypoint({
      BOT_IDENTITY_MODE: "single",
      BOT_AUTH: "form-login",
      BOT_PARTICIPANT: "k8s-bot-1",
      BOT_EMAIL: "solo@example.test",
      BOT_PASSWORD: "pw-solo",
    });
    expect(status).toBe(0);
    expect(stdout).toContain("STUB_ARGS=");
    expect(stdout).not.toContain("--bot-index");
  });

  it("passes --hardware-concurrency through when BOT_HW_CONCURRENCY is set, omits it when empty", () => {
    const withCap = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-0",
      BOT_HW_CONCURRENCY: "6",
      ...ORDINAL_ACCOUNTS,
    });
    expect(withCap.status).toBe(0);
    expect(withCap.stdout).toContain("--hardware-concurrency 6");

    const noCap = runEntrypoint({
      BOT_IDENTITY_MODE: "ordinal",
      BOT_AUTH: "form-login",
      HOSTNAME: "videocall-bots-0",
      BOT_HW_CONCURRENCY: "", // explicit empty omits the flag (escape hatch)
      ...ORDINAL_ACCOUNTS,
    });
    expect(noCap.status).toBe(0);
    expect(noCap.stdout).not.toContain("--hardware-concurrency");
  });
});

/**
 * Variant of {@link runEntrypoint} that keeps the workdir alive so the test can
 * inspect what the entrypoint left on disk, and whose stub echoes the token-dir
 * env var + the resolved `--assets-dir`. The callback runs with the workdir
 * still present; cleanup happens after it returns.
 *
 * Same production script, no reimplementation — reverting the #2157 sweep or the
 * `export BOT_CTL_STATE_DIR` in docker-entrypoint.sh fails the assertions below.
 */
function withEntrypointRun(
  vars: Record<string, string>,
  seed: (dirs: { runDir: string; stateDir: string }) => void,
  assert: (r: {
    status: number;
    stdout: string;
    stderr: string;
    runDir: string;
    stateDir: string;
  }) => void,
): void {
  const workdir = mkdtempSync(join(tmpdir(), "bot-entrypoint-ctl-"));
  const binDir = join(workdir, "node_modules", ".bin");
  mkdirSync(binDir, { recursive: true });
  // The no-profile path still probes, so an unstubbed `tc` reads the host's iface.
  const netBin = join(workdir, "netbin");
  mkdirSync(netBin, { recursive: true });
  const tcStub = join(netBin, "tc");
  writeFileSync(tcStub, `#!/usr/bin/env bash\nprintf '%s\\n' ${JSON.stringify(UNSHAPED_QDISC)}\n`);
  chmodSync(tcStub, 0o755);
  const stub = join(binDir, "tsx");
  writeFileSync(
    stub,
    [
      "#!/usr/bin/env bash",
      // Proves the var reached the exec'd process at all — which is what
      // auth.ts's resolveCtlStateDir() needs in order to keep the token off the
      // PVC. (It does NOT isolate the `export` statement: the var can only be
      // non-empty here by arriving already-exported from the container env, so
      // deleting that line does not regress this. See the note there.)
      'echo "STUB_CTL_STATE_DIR=${BOT_CTL_STATE_DIR-<unset>}"',
      'echo "STUB_ARGS=$*"',
      "",
    ].join("\n"),
  );
  chmodSync(stub, 0o755);

  const runDir = join(workdir, "run");
  const stateDir = join(workdir, "ctl-state");
  // Only runDir is pre-created (so a seed() can drop stale files in it). The
  // state dir is DELIBERATELY left absent: the entrypoint must `mkdir -p` it
  // itself, exactly as it will on a fresh emptyDir mount. Seeds that need it
  // create it themselves.
  mkdirSync(runDir, { recursive: true });
  seed({ runDir, stateDir });

  const env: Record<string, string> = {
    PATH: `${netBin}:${process.env.PATH ?? "/usr/bin:/bin"}`,
    HOME: workdir,
    BOT_RUN_DIR: runDir,
    // Mirrors the StatefulSet wiring (BOT_RUN_DIR = PVC, BOT_CTL_STATE_DIR =
    // emptyDir). `...vars` still wins, so a case can set it to "" to exercise
    // the unset / `docker run` fallback.
    BOT_CTL_STATE_DIR: stateDir,
    ...vars,
  };
  try {
    let status = 0;
    let stdout = "";
    let stderr = "";
    try {
      // `spawnSync`, not `execFileSync`: the latter RETURNS only stdout, so a
      // SUCCESSFUL run's stderr is unobservable — and this suite needs it,
      // because the stale-token sweep reports its non-fatal failures there and
      // that warning is the whole point of the `|| warn` path.
      const res = spawnSync("bash", [ENTRYPOINT], { cwd: workdir, env, encoding: "utf8" });
      status = res.status ?? 1;
      stdout = res.stdout ?? "";
      stderr = res.stderr ?? "";
    } catch (e) {
      const err = e as { status?: number; stdout?: string; stderr?: string };
      status = err.status ?? 1;
      stdout = err.stdout ?? "";
      stderr = err.stderr ?? "";
    }
    assert({ status, stdout, stderr, runDir, stateDir });
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
}

describe("docker-entrypoint.sh — ctl token lifetime (#2157)", () => {
  const ORDINAL_ENV = {
    BOT_IDENTITY_MODE: "ordinal",
    BOT_AUTH: "form-login",
    HOSTNAME: "videocall-bots-0",
    BOT_EMAIL_0: "alice@example.test",
    BOT_PASSWORD_0: "pw-alice",
  };

  it("sweeps a STALE ctl-*.token off the run dir at startup, keeping the #2032 CSVs", () => {
    // The pre-existing-PVC case: volumes provisioned by the #2154 deploy already
    // carry a token written before this fix, and a StatefulSet never reclaims
    // them. The startup sweep is what retires those copies.
    withEntrypointRun(
      { ...ORDINAL_ENV, BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "fleet-secret" },
      ({ runDir }) => {
        writeFileSync(join(runDir, "ctl-999.token"), '{"token":"stale-fleet-secret"}');
        writeFileSync(join(runDir, "ctl-1000.token"), '{"token":"stale-too"}');
        // #2032 artifacts that MUST survive the sweep — this is the persistence
        // win the fix must not regress.
        //
        // Seeded at their REAL production paths: `ResourceCaptureSession`
        // (session.ts) writes to `<runDir>/resource/<label>-*`, NOT the runDir
        // root. An earlier version of this test seeded them at the root, where
        // the non-recursive `ctl-*.token` glob makes them structurally immune —
        // so it asserted a strictly WEAKER case than production and would not
        // have noticed the sweep gaining `-r` or a `**` glob.
        mkdirSync(join(runDir, "resource"), { recursive: true });
        writeFileSync(join(runDir, "resource", "run-x-raw.csv"), "ts,cpu\n");
        writeFileSync(join(runDir, "resource", "run-x-derived.csv"), "ts,cpu\n");
        writeFileSync(join(runDir, "resource", "run-x-summary.txt"), "VERDICT: OK\n");
        // A token nested one level down must ALSO survive, for the same reason:
        // it pins that the sweep is deliberately non-recursive. If someone makes
        // the glob recursive, this file disappears and this test fails — which is
        // the signal to re-think the CSV interaction above.
        writeFileSync(join(runDir, "resource", "ctl-nested.token"), '{"token":"nested"}');
      },
      ({ status, runDir }) => {
        expect(status).toBe(0);
        const left = readdirSync(runDir).sort();
        // Every stale token in the swept dir is gone (the glob matched both) …
        expect(left.filter((f) => f.endsWith(".token"))).toEqual([]);
        // … and every resource artifact, at its real production path, untouched.
        const artifacts = readdirSync(join(runDir, "resource")).sort();
        expect(artifacts).toContain("run-x-raw.csv");
        expect(artifacts).toContain("run-x-derived.csv");
        expect(artifacts).toContain("run-x-summary.txt");
        // Non-recursive: the nested token is NOT swept.
        expect(artifacts).toContain("ctl-nested.token");
      },
    );
  });

  it("starts the bot even when a stale token sits in a dir it cannot write", () => {
    // The sweep is best-effort cleanup and must NEVER stop the bot from starting.
    // Without `|| true` this crashloops: `rm` exits 1 on an unwritable dir, and
    // because it is the FINAL command of the `[ … ] && rm …` AND-list, `set -e`
    // kills the script — the pod's only output being a bare
    // "rm: cannot remove …: Permission denied". `mkdir -p` does not catch it
    // first (on an existing read-only dir it returns 0).
    withEntrypointRun(
      { ...ORDINAL_ENV, BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "fleet-secret" },
      ({ runDir }) => {
        writeFileSync(join(runDir, "ctl-999.token"), '{"token":"stale"}');
        // r-x: the token is visible but cannot be unlinked (unlink needs write
        // on the DIRECTORY, not the file).
        chmodSync(runDir, 0o500);
      },
      ({ status, stdout, stderr, runDir }) => {
        // Restore the mode first so the fixture teardown can remove the dir.
        chmodSync(runDir, 0o700);
        // Reached the exec, i.e. the sweep failure did not abort startup.
        expect(status).toBe(0);
        expect(stdout).toContain("STUB_CTL_STATE_DIR=");
        // …and it did NOT fail silently. A silent failure is the worst outcome:
        // the stale cleartext token stays on the PVC while README.md tells the
        // operator the sweep retired it. `rm`'s own stderr ("cannot remove …:
        // Permission denied") says nothing about that consequence, so the
        // entrypoint must name the dir AND the security implication.
        const log = stdout + stderr;
        expect(log).toContain("could not sweep stale ctl-*.token");
        expect(log).toContain(runDir);
        expect(log).toMatch(/rotating the bot-ctl-token Secret will NOT retire that copy/i);
      },
    );
  });

  it("puts BOT_CTL_STATE_DIR in the exec'd process env while --assets-dir stays the PVC", () => {
    // The load-bearing split: if the token dir did NOT reach the Node process,
    // resolveCtlStateDir() would fall back to --assets-dir (the retained PVC) —
    // the exact bug this fix exists to prevent. Asserting on the CHILD's env is
    // the only way to see it; a parent-side check would pass either way.
    withEntrypointRun(
      { ...ORDINAL_ENV, BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "fleet-secret" },
      () => {},
      ({ status, stdout, stateDir, runDir }) => {
        expect(status).toBe(0);
        expect(stdout).toContain(`STUB_CTL_STATE_DIR=${stateDir}`);
        // --assets-dir still points at the PVC, so the CSVs keep persisting.
        expect(stdout).toContain(`--assets-dir ${runDir}`);
        // The startup line reports the DIRECTORY, never the token value.
        expect(stdout).toContain(`token_dir=${stateDir}`);
        expect(stdout).not.toContain("fleet-secret");
      },
    );
  });

  it("also sweeps the state dir when it is a distinct directory", () => {
    withEntrypointRun(
      { ...ORDINAL_ENV, BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "fleet-secret" },
      ({ stateDir }) => {
        mkdirSync(stateDir, { recursive: true });
        writeFileSync(join(stateDir, "ctl-321.token"), '{"token":"stale"}');
      },
      ({ status, stateDir }) => {
        expect(status).toBe(0);
        expect(readdirSync(stateDir).filter((f) => f.endsWith(".token"))).toEqual([]);
      },
    );
  });

  it("leaves the token in the run dir when BOT_CTL_STATE_DIR is unset (docker run default)", () => {
    // Back-compat: the non-K8s path must keep working unchanged.
    withEntrypointRun(
      {
        ...ORDINAL_ENV,
        BOT_CTL_PORT: "8080",
        BOT_CTL_TOKEN: "fleet-secret",
        BOT_CTL_STATE_DIR: "",
      },
      () => {},
      ({ status, stdout, runDir }) => {
        expect(status).toBe(0);
        expect(stdout).toContain("STUB_CTL_STATE_DIR=");
        expect(stdout).toContain(`token_dir=${runDir}`);
      },
    );
  });

  it("sweeps unconditionally — even with the control server DISABLED", () => {
    // A pod whose ctl is now off must still shed a token left by an earlier
    // ctl-enabled run. Gating the sweep on BOT_CTL_PORT would strand it.
    withEntrypointRun(
      ORDINAL_ENV, // no BOT_CTL_PORT ⇒ control server disabled
      ({ runDir }) => {
        writeFileSync(join(runDir, "ctl-555.token"), '{"token":"stale-from-earlier-run"}');
      },
      ({ status, stdout, runDir }) => {
        expect(status).toBe(0);
        expect(readdirSync(runDir).filter((f) => f.endsWith(".token"))).toEqual([]);
        expect(stdout).toContain("control=[disabled]");
      },
    );
  });

  it("survives an EMPTY run dir — the glob matching nothing is a no-op, not an error", () => {
    // The `set -euo pipefail` failure path: `rm -f` on an unmatched glob must
    // exit 0 and the `[ -n ]` AND-list must not abort the script.
    withEntrypointRun(
      { ...ORDINAL_ENV, BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "fleet-secret" },
      () => {},
      ({ status, stdout }) => {
        expect(status).toBe(0);
        expect(stdout).toContain("STUB_ARGS=");
      },
    );
  });
});

describe("docker-entrypoint.sh — startup netem + join stagger (#2354)", () => {
  const ORDINAL_ENV = {
    BOT_IDENTITY_MODE: "ordinal",
    BOT_AUTH: "form-login",
    HOSTNAME: "videocall-bots-0",
    BOT_EMAIL_0: "alice@example.test",
    BOT_PASSWORD_0: "pw-alice",
    BOT_EMAIL_1: "bob@example.test",
    BOT_PASSWORD_1: "pw-bob",
  };

  const flagValue = (stdout: string, flag: string): string | undefined => {
    const args = /^STUB_ARGS=(.*)$/m.exec(stdout)?.[1].split(/\s+/) ?? [];
    const i = args.indexOf(flag);
    return i < 0 ? undefined : args[i + 1];
  };

  const line = (c: NetemCommand): string => [c.file, ...c.args].join(" ");

  /** The post-read; it reads the ifb device too whenever a hook is present. */
  const expectedRead = (iface = NETEM_IFACE_DEFAULT, mirror = false): string[] => [
    `tc ${buildNetemProbeArgs(iface).join(" ")}`,
    ...(mirror ? [`tc ${buildNetemProbeArgs(NETEM_IFB_DEV).join(" ")}`] : []),
  ];

  /** Every argv comes from the production builders, so a shell/TS drift fails. */
  const expectedShape = (
    params: NonNullable<(typeof NETEM_PROFILES)[string]>,
    iface = NETEM_IFACE_DEFAULT,
    ifbExists = false,
  ): string[] => [
    `tc ${buildNetemShapeArgs(iface, params).join(" ")}`,
    ...buildNetemMirrorInstallArgs(iface, params)
      .filter((_, i) => !(ifbExists && i === NETEM_MIRROR_ADD_STEP))
      .map(line),
    ...expectedRead(iface, true),
  ];

  const expectedClear = (iface = NETEM_IFACE_DEFAULT, mirror = false): string[] => {
    const teardown = buildNetemMirrorClearArgs(iface);
    return [
      `tc ${buildNetemClearArgs(iface).join(" ")}`,
      ...(mirror ? teardown : teardown.slice(0, 1)).map(line),
      ...expectedRead(iface),
    ];
  };

  it.each(NETEM_PROFILE_NAMES)("shapes BOTH directions from startup for profile %s", (name) => {
    const params = NETEM_PROFILES[name];
    const { status, stdout, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: name,
    });
    expect(status, `profile ${name} must be known to the shell table`).toBe(0);
    expect(commands).toEqual(params === null ? expectedClear() : expectedShape(params));
    expect(stdout).toContain("netem=[");
    expect(stdout.indexOf("netem=[")).toBeLessThan(stdout.indexOf("STUB_ARGS="));
    expect(stdout).toContain(`profile=${name}`);
    // `netem` with no parameters would report an impairment that is not applied.
    if (params !== null) expect(commands[0]).toMatch(/ root netem .+/);
  });

  it.each(
    NETEM_PROFILE_NAMES.filter(
      (n) =>
        NETEM_PROFILES[n] !== null &&
        NETEM_PROFILES[n]!.downlinkRateKbit !== NETEM_PROFILES[n]!.rateKbit,
    ),
  )("shapes %s's ingress at its downlink rate, not its uplink rate", (name) => {
    const params = NETEM_PROFILES[name]!;
    const { status, commands } = runEntrypoint({ ...ORDINAL_ENV, BOT_NETEM_PROFILE: name });
    expect(status).toBe(0);
    const ifb = commands.find((c) => c.startsWith(`tc qdisc replace dev ${NETEM_IFB_DEV} root`));
    expect(ifb, "the mirror must carry a netem").toBeDefined();
    expect(ifb).toContain(`rate ${params.downlinkRateKbit}kbit`);
    expect(ifb).not.toContain(`rate ${params.rateKbit}kbit`);
  });

  it("reports the interface, both directions, and every netem parameter it applied", () => {
    const iface = "eth1";
    const egress = buildNetemShapeArgs(iface, NETEM_PROFILES.good_4g!);
    const ingress = buildNetemShapeArgs(NETEM_IFB_DEV, ingressNetemParams(NETEM_PROFILES.good_4g!));
    const params = (argv: string[]): string => argv.slice(argv.indexOf("netem") + 1).join(" ");
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "good_4g",
      BOT_NETEM_IFACE: iface,
    });
    expect(status).toBe(0);
    expect(stdout).toContain(
      `netem=[shape profile=good_4g iface=${iface} direction=both egress=[${params(egress)}] ingress=[dev ${NETEM_IFB_DEV} ${params(ingress)}]]`,
    );
  });

  const probeOnly = (): string[] => [`tc qdisc show dev ${NETEM_IFACE_DEFAULT}`];

  it("mutates nothing when BOT_NETEM_PROFILE is unset, and does not claim the link is clean", () => {
    const { status, stdout, commands } = runEntrypoint(ORDINAL_ENV);
    expect(status).toBe(0);
    expect(commands).toEqual(probeOnly());
    expect(stdout).toContain(
      `netem=[no-netem iface=${NETEM_IFACE_DEFAULT} tc=[${UNSHAPED_QDISC}] ingress=none]`,
    );
  });

  it("reports a qdisc it inherited instead of describing a shaped link as unmanaged", () => {
    const netem = "qdisc netem 8001: root refcnt 2 limit 1000 delay 80ms 30ms loss 2% rate 2Mbit";
    const { status, stdout, stderr, commands } = runEntrypoint(ORDINAL_ENV, { qdiscShow: netem });
    expect(status).toBe(0);
    expect(commands, "detection must not mutate the interface").toEqual(probeOnly());
    expect(stdout).toContain(`netem=[inherited iface=${NETEM_IFACE_DEFAULT}`);
    expect(stdout).not.toContain("netem=[no-netem");
    expect(stdout, "an inherited profile's parameters must not be discarded").toContain(
      `tc=[${netem}]`,
    );
    expect(stderr).toContain("neither applied nor cleared it");
  });

  it("does not read a non-netem qdisc as inherited shaping", () => {
    const { status, stdout, stderr, commands } = runEntrypoint(ORDINAL_ENV, {
      qdiscShow: UNSHAPED_QDISC,
    });
    expect(status).toBe(0);
    expect(commands, "the probe must still run").toEqual(probeOnly());
    expect(stdout).toContain(`netem=[no-netem iface=${NETEM_IFACE_DEFAULT}`);
    expect(stderr).not.toContain("inherited");
  });

  // A probe that found no netem has not established that nothing shapes the link,
  // and a shaped pod reporting a clean one is the false-green direction.
  it("carries the qdisc it did find, so a non-netem rate limiter is still visible", () => {
    const tbf = "qdisc tbf 8002: root refcnt 2 rate 1Mbit burst 5Kb lat 10ms";
    const { status, stdout } = runEntrypoint(ORDINAL_ENV, { qdiscShow: tbf });
    expect(status).toBe(0);
    expect(stdout, "the evidence must reach the launch line").toContain(`tc=[${tbf}]`);
  });

  // Fixture exceeds 64KiB on purpose: a `tc … | grep -q` probe SIGPIPEs the
  // writer once grep exits early, and `pipefail` then reads a shaped link as
  // unshaped. Only a pipeline-free probe survives this.
  it("reads a qdisc listing larger than a pipe buffer without misreporting it", () => {
    const netem = "qdisc netem 8001: root refcnt 2 limit 1000 delay 80ms 30ms loss 2% rate 2Mbit";
    const filler = Array.from(
      { length: 900 },
      (_, i) =>
        `qdisc fq_codel ${i}: parent 1:${i} limit 10240p flows 1024 quantum 1514 target 5ms`,
    ).join("\n");
    const { status, stdout } = runEntrypoint(ORDINAL_ENV, { qdiscShow: `${netem}\n${filler}` });
    expect(status).toBe(0);
    expect(stdout).toContain(`netem=[inherited iface=${NETEM_IFACE_DEFAULT}`);
    expect(stdout, "the netem line must survive a listing this long").toContain(`tc=[${netem}]`);
  });

  it("reports the shaping root alongside a netem nested under it", () => {
    const root = "qdisc tbf 8002: root refcnt 2 rate 1Mbit burst 5Kb lat 10ms";
    const netem = "qdisc netem 8003: parent 8002:1 limit 1000 delay 80ms";
    const { stdout } = runEntrypoint(ORDINAL_ENV, { qdiscShow: `${root}\n${netem}` });
    expect(stdout, "a root that sets the rate must not be dropped").toContain(
      `tc=[${root}; ${netem}]`,
    );
  });

  it.each([
    [127, "tc: command not found"],
    [1, 'Cannot find device "eth0"'],
  ])("refuses to call the link unmanaged when the probe itself failed (rc=%i)", (rc, err) => {
    const { status, stdout, stderr, commands } = runEntrypoint(ORDINAL_ENV, {
      failWhen: "qdisc show",
      failStatus: rc,
      failStderr: err,
    });
    expect(status, "an unreadable qdisc is a disclosure problem, not a fatal one").toBe(0);
    expect(commands).toEqual(probeOnly());
    expect(stdout, "a failed probe supports no claim about the link").not.toMatch(
      /netem=\[(no-netem|inherited)/,
    );
    expect(stdout).toContain(`netem=[unread iface=${NETEM_IFACE_DEFAULT} probe-failed rc=${rc}]`);
    expect(stderr, "the operator must see WHY the posture is unknown").toContain(err);
  });

  it("lets one ordinal carry a different link than the fleet", () => {
    const fleet = { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "good_4g", BOT_NETEM_PROFILE_1: "dialup" };
    const overridden = runEntrypoint({ ...fleet, HOSTNAME: "videocall-bots-1" });
    expect(overridden.status).toBe(0);
    expect(overridden.commands).toEqual(expectedShape(NETEM_PROFILES.dialup!));
    expect(overridden.stdout).toContain("profile=dialup");
    // Ordinal 0 keeps the fleet-wide profile: indirection, not a blanket override.
    const untouched = runEntrypoint({ ...fleet, HOSTNAME: "videocall-bots-0" });
    expect(untouched.status).toBe(0);
    expect(untouched.commands).toEqual(expectedShape(NETEM_PROFILES.good_4g!));
  });

  it("resolves ordinal 0's OWN override, not just the fleet-wide value", () => {
    const { status, stdout, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "good_4g",
      BOT_NETEM_PROFILE_0: "dialup",
    });
    expect(status).toBe(0);
    expect(commands).toEqual(expectedShape(NETEM_PROFILES.dialup!));
    expect(stdout).toContain("profile=dialup");
  });

  it("treats an explicitly EMPTY per-ordinal profile as 'no profile', NOT as a clear", () => {
    const { status, stdout, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_NETEM_PROFILE: "good_4g",
      BOT_NETEM_PROFILE_1: "",
    });
    expect(status).toBe(0);
    expect(commands).toEqual(probeOnly());
    expect(stdout).toContain(`netem=[no-netem iface=${NETEM_IFACE_DEFAULT}`);
  });

  it("clears one ordinal's link on demand, which empty cannot do", () => {
    const { status, stdout, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_NETEM_PROFILE: "good_4g",
      BOT_NETEM_PROFILE_1: "clean",
    });
    expect(status).toBe(0);
    expect(commands).toEqual(expectedClear());
    expect(stdout).toContain("profile=clean");
  });

  it("refuses to start on an unknown profile instead of joining unshaped", () => {
    const { status, stdout, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "congested-wifi", // hyphen, not underscore
    });
    expect(status).toBe(1);
    expect(stderr).toContain("unknown BOT_NETEM_PROFILE");
    expect(commands).toEqual([]);
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it("only names profiles the control server also knows", () => {
    const script = readFileSync(ENTRYPOINT, "utf8");
    const block = /case "\$\{BOT_NETEM_PROFILE\}" in\n([\s\S]*?)\n\s*esac/.exec(script);
    expect(block, "the profile case block must be findable").not.toBeNull();
    const names = [...block![1].matchAll(/^\s*([a-z0-9_ |]+)\)/gm)].flatMap((m) =>
      m[1].split("|").map((s) => s.trim()),
    );
    expect(names.length).toBeGreaterThan(0);
    expect([...names].sort()).toEqual([...NETEM_PROFILE_NAMES].sort());
    // The rejection message lists them separately, so it drifts separately.
    const known = /Known: ([a-z0-9_ ]+)\(src\/control\/netem\.ts\)/.exec(script);
    expect(known, "the unknown-profile message must list the known names").not.toBeNull();
    expect(known![1].trim().split(/\s+/).sort()).toEqual([...NETEM_PROFILE_NAMES].sort());
  });

  const MIRROR_ENV = { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "congested_wifi" };

  it("reaches ip only through the capped setpriv, never bare on PATH (#2428)", () => {
    const { status, commands } = runEntrypoint({ ...MIRROR_ENV, NETEM_SETPRIV: "" });
    expect(commands.filter((c) => c.startsWith("ip "))).toEqual([]);
    expect(status, "a link that does not match the profile is the false green").toBe(1);
  });

  it("reuses an ifb0 the pod netns kept across a container restart", () => {
    // `ip link add` over an existing device fails; a restart must not crashloop.
    const { status, commands } = runEntrypoint(MIRROR_ENV, { preexistingIfb: true });
    expect(status).toBe(0);
    expect(commands).toEqual(
      expectedShape(NETEM_PROFILES.congested_wifi!, NETEM_IFACE_DEFAULT, true),
    );
    expect(commands.filter((c) => c.includes("link add"))).toEqual([]);
  });

  it("re-shapes over an ingress hook a prior process left behind", () => {
    // `tc qdisc add … ingress` refuses to modify an existing hook, so the hook
    // is deleted first — which is also what discards its stale u32 filter.
    const { status, commands } = runEntrypoint(MIRROR_ENV, {
      preexistingIfb: true,
      preexistingHook: true,
    });
    expect(status).toBe(0);
    expect(commands).toEqual(
      expectedShape(NETEM_PROFILES.congested_wifi!, NETEM_IFACE_DEFAULT, true),
    );
  });

  it("removes all three mirror objects on a clear, hook first", () => {
    const { status, stdout, commands } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
      { preexistingIfb: true, preexistingHook: true },
    );
    expect(status).toBe(0);
    expect(commands).toEqual(expectedClear(NETEM_IFACE_DEFAULT, true));
    expect(stdout).toContain(`netem=[clear profile=clean iface=${NETEM_IFACE_DEFAULT}`);
    expect(stdout).toContain("ingress=none");
  });

  it("leaves ifb0 alone when this interface carries no hook to remove", () => {
    const { status, commands } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
      { preexistingIfb: true },
    );
    expect(status).toBe(0);
    expect(commands).toEqual(expectedClear());
    expect(commands.filter((c) => c.includes(NETEM_IFB_DEV))).toEqual([]);
  });

  it.each([
    ["ip link add ifb0", "link add"],
    ["bringing ifb0 up", "link set ifb0 up"],
    ["the ifb device queue depth", "txqueuelen"],
    ["the ingress hook", "qdisc add dev eth0 handle"],
    ["the u32 redirect filter", "filter add"],
    ["the ifb netem", `qdisc replace dev ${NETEM_IFB_DEV} root`],
  ])("refuses to start when %s fails, rather than joining half-shaped", (_case, failWhen) => {
    const { status, stdout, stderr } = runEntrypoint(MIRROR_ENV, { failWhen });
    expect(status, "a shaped uplink with a line-rate downlink is the false green").toBe(1);
    expect(stderr).toContain("docker-entrypoint: FATAL");
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it("refuses to start when the post-read finds no netem it just applied", () => {
    // Every command reports success; only the read disproves it.
    const { status, stderr, stdout } = runEntrypoint(MIRROR_ENV, { qdiscShow: UNSHAPED_QDISC });
    expect(status).toBe(1);
    expect(stderr).toContain("post-read found no netem");
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it("refuses to start when the post-read finds no ingress mirror", () => {
    const netem = "qdisc netem 8001: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 2Mbit";
    const { status, stderr } = runEntrypoint(MIRROR_ENV, { qdiscShow: netem });
    expect(status).toBe(1);
    expect(stderr).toContain("post-read found no ingress netem");
  });

  it("refuses a mirror whose device carries no netem, which the hook alone hides", () => {
    // The hook is installed and redirects, but nothing shapes what it carries.
    const netem = "qdisc netem 8001: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 2Mbit";
    const { status, stderr } = runEntrypoint(MIRROR_ENV, {
      qdiscShow: `${netem}\nqdisc ingress ffff: parent ffff:fff1`,
      ifbQdiscShow: UNSHAPED_QDISC,
    });
    expect(status).toBe(1);
    expect(stderr).toContain(`post-read found no ingress netem on ${NETEM_IFB_DEV}`);
  });

  it.each([
    ["a netem qdisc", "qdisc netem 8001: root refcnt 2 limit 55 delay 80ms", "still shows netem"],
    [
      "an ingress mirror",
      `${UNSHAPED_QDISC}\nqdisc ingress ffff: parent ffff:fff1`,
      "still shows an ingress mirror",
    ],
  ])("refuses to report a clear the post-read shows left %s", (_case, qdiscShow, reason) => {
    const { status, stdout, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
      { qdiscShow },
    );
    expect(status, "a reported clear that did not clear is the API lying").toBe(1);
    expect(stderr).toContain(reason);
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it("refuses to start when the post-read itself cannot run", () => {
    const { status, stderr } = runEntrypoint(MIRROR_ENV, {
      failWhen: "qdisc show",
      failStatus: 1,
      failStderr: 'Cannot find device "eth0"',
    });
    expect(status, "an unverifiable mutation supports no receipt").toBe(1);
    expect(stderr).toContain("tc qdisc show dev eth0 after shape");
  });

  it("discloses a mirror on the no-profile path, where the root qdisc hides it", () => {
    // A shaped downlink with an unshaped uplink is exactly what `no-netem`
    // would otherwise have described as an unshaped link.
    const ifb = "qdisc netem 8002: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 4Mbit";
    const { status, stdout, stderr, commands } = runEntrypoint(ORDINAL_ENV, {
      qdiscShow: `${UNSHAPED_QDISC}\nqdisc ingress ffff: parent ffff:fff1`,
      ifbQdiscShow: ifb,
    });
    expect(status).toBe(0);
    expect(commands, "detection must not mutate the interface").toEqual(
      expectedRead(NETEM_IFACE_DEFAULT, true),
    );
    expect(stdout).toContain(`netem=[inherited iface=${NETEM_IFACE_DEFAULT}`);
    expect(stdout).toContain(`ingress=[${ifb}]`);
    expect(stdout).not.toContain("ingress=none");
    expect(stderr).toContain("neither applied nor cleared it");
  });

  it("says the ifb read failed rather than reporting an unshaped downlink", () => {
    const { status, stdout } = runEntrypoint(ORDINAL_ENV, {
      qdiscShow: `${UNSHAPED_QDISC}\nqdisc ingress ffff: parent ffff:fff1`,
    });
    expect(status).toBe(0);
    expect(stdout).toContain('ingress=[Cannot find device "ifb0"]');
  });

  it("refuses to start when tc cannot shape, rather than joining unshaped", () => {
    const { status, stdout, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "congested_wifi" },
      { failWhen: `qdisc replace dev ${NETEM_IFACE_DEFAULT} root netem` },
    );
    expect(status).toBe(1);
    expect(stderr).toMatch(/FATAL .* qdisc replace/);
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it("mirrors the control server's benign-clear set exactly, in both directions", () => {
    const script = readFileSync(ENTRYPOINT, "utf8");
    const block = /case "\$\{netem_err_lc\}" in\n([\s\S]*?)\n\s*esac/.exec(script);
    expect(block, "the benign-clear case block must be findable").not.toBeNull();
    const arms = block![1].split("\n").slice(
      0,
      block![1].split("\n").findIndex((l) => /^\s*\*\)/.test(l)),
    );
    expect(arms.length, "the tolerated arm must precede the catch-all").toBeGreaterThan(0);
    const patterns = [...arms.join("\n").matchAll(/\*"([^"]+)"\*/g)].map((m) => m[1]);
    // Set equality, not containment: a needle on one side only is drift either way.
    expect([...patterns].sort()).toEqual([...NETEM_BENIGN_CLEAR_ERRORS].sort());
  });

  it("tolerates exactly the clear failures the control server calls benign", () => {
    for (const benign of NETEM_BENIGN_CLEAR_ERRORS) {
      const { status, commands } = runEntrypoint(
        { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
        { failWhen: "qdisc del", failStderr: `Error: ${benign.toUpperCase()} qdisc, whatever` },
      );
      expect(status, `'${benign}' must not abort startup`).toBe(0);
      expect(commands).toEqual(expectedClear());
    }
  });

  it("fails closed when the clear fails for any other reason", () => {
    const { status, stdout, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
      { failWhen: "qdisc del", failStderr: "RTNETLINK answers: Operation not permitted" },
    );
    expect(status).toBe(1);
    expect(stderr).toContain("Operation not permitted");
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it.each([126, 127])("fails closed when tc never executed (rc=%i)", (failStatus) => {
    // A loader failure's own diagnostic contains "no such file", a benign-clear
    // needle — so status must be judged before wording.
    const { status, stdout, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean" },
      {
        failWhen: "qdisc del",
        failStatus,
        failStderr: "/usr/sbin/tc: /lib/ld.so: bad interpreter: No such file or directory",
      },
    );
    expect(status, "an unexecutable tc must not read as a successful clear").toBe(1);
    expect(stderr).toContain(`rc=${failStatus}`);
    expect(stdout).not.toContain("netem applied");
    expect(stdout).not.toContain("STUB_ARGS=");
  });

  it.each([
    "eth 0",
    "eth0\nnetem applied — shape profile=good_wifi",
    "thisnameistoolong",
    "-eth0",
    "..",
  ])("refuses an invalid BOT_NETEM_IFACE (%j) before it reaches tc", (iface) => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "good_wifi",
      BOT_NETEM_IFACE: iface,
    });
    expect(status).toBe(1);
    expect(stderr).toContain("is not a valid device name");
    expect(commands, "no tc may run on an unvalidated device name").toEqual([]);
  });

  it("refuses an invalid BOT_NETEM_IFACE with no profile set, before the probe reads it", () => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_IFACE: "-eth0",
    });
    expect(status).toBe(1);
    expect(stderr).toContain("is not a valid device name");
    expect(commands, "no tc may run on an unvalidated device name").toEqual([]);
  });

  it("accepts the same device names netem.ts accepts", () => {
    const shell = /BOT_NETEM_IFACE\}" =~ \^(\S+)\$ \]\]/.exec(readFileSync(ENTRYPOINT, "utf8"));
    expect(shell, "the iface grammar must be findable in the entrypoint").not.toBeNull();
    // Set equality against the production regex: a grammar that drifts either way
    // lets a value through one side and not the other.
    expect(`^${shell![1]}$`).toBe(IFACE_PATTERN.source);
  });

  // netem_read gates the ifb read on this literal, so a drift makes the
  // no-profile branch report ingress=none on a pod whose downlink IS mirrored.
  it.each([
    ["NETEM_INGRESS_MARKER", () => NETEM_INGRESS_QDISC_MARKER],
    ["NETEM_IFB_DEV", () => NETEM_IFB_DEV],
    ["NETEM_IFB_TXQUEUELEN", () => String(NETEM_IFB_TXQUEUELEN)],
  ])("keeps the shell's %s equal to netem.ts's", (name, expected) => {
    const shell = new RegExp(`^${name}="([^"]*)"$`, "m").exec(readFileSync(ENTRYPOINT, "utf8"));
    expect(shell, `${name} must be findable in the entrypoint`).not.toBeNull();
    expect(shell![1]).toBe(expected());
  });

  // The entrypoint shapes its own default; cli.ts falls back to netem.ts's when
  // the var is unset. Divergence would clear a different device than it shaped.
  it("defaults to the same interface netem.ts does", () => {
    const shell = /^BOT_NETEM_IFACE="\$\{BOT_NETEM_IFACE:-(\S+)\}"$/m.exec(
      readFileSync(ENTRYPOINT, "utf8"),
    );
    expect(shell, "the iface default must be findable in the entrypoint").not.toBeNull();
    expect(shell![1]).toBe(NETEM_IFACE_DEFAULT);
  });

  it("aborts on a misnamed BOT_NETEM_IFACE rather than reporting a clean link", () => {
    // A device that does not exist is not an already-clean qdisc.
    const { status, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_NETEM_PROFILE: "clean", BOT_NETEM_IFACE: "eth9" },
      { failWhen: "qdisc del", failStderr: 'Cannot find device "eth9"' },
    );
    expect(status).toBe(1);
    expect(stderr).toContain("Cannot find device");
  });

  it("shapes the interface BOT_NETEM_IFACE names", () => {
    const { status, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "good_4g",
      BOT_NETEM_IFACE: "eth1",
    });
    expect(status).toBe(0);
    expect(commands[0]).toBe(
      `tc ${buildNetemShapeArgs("eth1", NETEM_PROFILES.good_4g!).join(" ")}`,
    );
  });

  const slept = (commands: string[]): number[] =>
    commands.filter((c) => c.startsWith("sleep ")).map((c) => Number(c.split(" ")[1]));

  it("reports no stagger as 'off', not as a drawn zero", () => {
    const { status, stdout, commands } = runEntrypoint(ORDINAL_ENV);
    expect(status).toBe(0);
    expect(slept(commands)).toEqual([]);
    expect(stdout).toMatch(/join_stagger=off /m);
  });

  it("staggers the join by 0..N seconds, after every preflight", () => {
    const max = 300;
    const { status, stdout, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_MAX_JOIN_STAGGER_SECS: String(max),
    });
    expect(status).toBe(0);
    expect(slept(commands)).toHaveLength(1);
    const secs = slept(commands)[0];
    expect(secs).toBeGreaterThanOrEqual(0);
    expect(secs).toBeLessThanOrEqual(max);
    expect(stdout).toContain(`join stagger — sleeping ${secs}s (max ${max}s)`);
    // End of line: a completed wait carries no qualifier.
    expect(stdout).toMatch(new RegExp(`join_stagger=${secs}s `, "m"));
    // The launch line comes after the sleep, or the log reads as a silent bot.
    expect(stdout.indexOf("join stagger")).toBeLessThan(stdout.indexOf("launching bot"));
    expect(stdout.indexOf("launching bot")).toBeLessThan(stdout.indexOf("STUB_ARGS="));
  });

  /** Signals mid-stagger. Death BY the signal reports `signal`; a trap reports a code. */
  const STAGGER_STUB_BLOCK_SECS = 5;
  function signalDuringStagger(sig: NodeJS.Signals): Promise<{
    code: number | null;
    signal: string | null;
    elapsedMs: number;
    stdout: string;
  }> {
    const workdir = mkdtempSync(join(tmpdir(), "bot-entrypoint-term-"));
    const binDir = join(workdir, "node_modules", ".bin");
    const netBin = join(workdir, "netbin");
    mkdirSync(binDir, { recursive: true });
    mkdirSync(netBin, { recursive: true });
    const sleepStub = join(netBin, "sleep");
    // A bare `sleep` here would re-exec this stub (netBin leads PATH); the reset
    // PATH resolves the real one.
    writeFileSync(
      sleepStub,
      `#!/usr/bin/env bash\nexec env PATH=/usr/bin:/bin sleep ${STAGGER_STUB_BLOCK_SECS}\n`,
    );
    chmodSync(sleepStub, 0o755);
    const stub = join(binDir, "tsx");
    writeFileSync(stub, '#!/usr/bin/env bash\necho "STUB_ARGS=$*"\n');
    chmodSync(stub, 0o755);

    return new Promise((resolve) => {
      const started = Date.now();
      const child = spawn("bash", [ENTRYPOINT], {
        cwd: workdir,
        env: {
          PATH: `${netBin}:${process.env.PATH ?? "/usr/bin:/bin"}`,
          HOME: workdir,
          BOT_RUN_DIR: join(workdir, "run"),
          ...ORDINAL_ENV,
          BOT_MAX_JOIN_STAGGER_SECS: "1",
        },
      });
      let stdout = "";
      let signalled = false;
      child.stdout.on("data", (chunk: Buffer) => {
        stdout += chunk.toString();
        if (!signalled && stdout.includes("join stagger")) {
          signalled = true;
          child.kill(sig);
        }
      });
      // `exit`, not `close`: the orphaned stub sleep holds the pipes open.
      child.on("exit", (code, signal) => {
        rmSync(workdir, { recursive: true, force: true });
        resolve({ code, signal, elapsedMs: Date.now() - started, stdout });
      });
    });
  }

  // A trapped exit, not death by the signal: the container's PID 1 must own the stop.
  it.each([
    ["SIGTERM", 143],
    ["SIGINT", 130],
  ] as const)(
    "terminates promptly on %s mid-stagger instead of ignoring it",
    async (sig, expectedCode) => {
      const { code, signal, elapsedMs, stdout } = await signalDuringStagger(sig);
      expect(stdout).toContain("join stagger");
      expect(signal).toBeNull();
      expect(code).toBe(expectedCode);
      expect(elapsedMs).toBeLessThan(STAGGER_STUB_BLOCK_SECS * 1000);
      expect(stdout).not.toContain("launching bot");
    },
    STAGGER_STUB_BLOCK_SECS * 3000,
  );

  const DRAW_ATTEMPTS = 25;

  it(
    "can draw the configured maximum itself, not max-1",
    () => {
      // `RANDOM % max` never reaches max, so with max=1 it would be pinned at 0.
      const draws = new Set<number>();
      for (let i = 0; i < DRAW_ATTEMPTS; i++) {
        const { status, commands } = runEntrypoint({
          ...ORDINAL_ENV,
          BOT_MAX_JOIN_STAGGER_SECS: "1",
        });
        expect(status).toBe(0);
        draws.add(slept(commands)[0]);
      }
      expect([...draws].sort()).toEqual([0, 1]);
    },
    DRAW_ATTEMPTS * 4000,
  );

  it.each([
    ["09", "(max 9s)"],
    ["32767", "(max 32767s)"],
  ])("accepts the stagger %j — base 10, and the ceiling itself", (value, line) => {
    const { status, stdout } = runEntrypoint({ ...ORDINAL_ENV, BOT_MAX_JOIN_STAGGER_SECS: value });
    expect(status).toBe(0);
    expect(stdout).toContain(line);
  });

  // 18446744073709551620 is 2^64+4, which wraps to 4 — a range check alone reads it as 4s.
  it.each([
    ["32768", "exceeds the 32767s"],
    ["18446744073709551620", "at most 5 digits"],
    ["0000000000009", "at most 5 digits"],
    ["5m", "must be a non-negative integer"],
    ["-1", "must be a non-negative integer"],
    ["0[$(id)]", "must be a non-negative integer"],
  ])("rejects the stagger %j rather than treating it as no stagger", (value, reason) => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_MAX_JOIN_STAGGER_SECS: value,
    });
    expect(status).toBe(1);
    expect(stderr).toContain("BOT_MAX_JOIN_STAGGER_SECS");
    expect(stderr).toContain(reason);
    expect(slept(commands)).toEqual([]);
  });

  it("still launches the bot when the stagger sleep itself fails, and says so on the launch line", () => {
    // `wait` returns the sleep's non-zero status; unguarded, `set -e` would abort
    // the pod at startup instead of joining the meeting.
    const { status, stdout, stderr } = runEntrypoint(
      { ...ORDINAL_ENV, BOT_MAX_JOIN_STAGGER_SECS: "3" },
      { failWhen: "sleep", failStderr: "sleep: cannot read realtime clock" },
    );
    expect(status).toBe(0);
    expect(stdout).toContain("STUB_ARGS=");
    expect(stderr).toContain("stagger sleep failed");
    expect(stdout).toMatch(/join_stagger=\d+s\(INCOMPLETE\) /m);
  });

  it("does not sleep when the stagger is unset or 0", () => {
    const unset = runEntrypoint(ORDINAL_ENV);
    expect(slept(unset.commands)).toEqual([]);
    expect(unset.stdout).not.toContain("join stagger");
    const zero = runEntrypoint({ ...ORDINAL_ENV, BOT_MAX_JOIN_STAGGER_SECS: "0" });
    expect(zero.status).toBe(0);
    expect(slept(zero.commands)).toEqual([]);
    expect(zero.stdout).not.toContain("join stagger");
  });

  // Both inputs are validated before the netem block, so a rejected config
  // cannot leave a shaped interface behind for the lifetime of a crash loop.
  it("rejects a bad stagger BEFORE tc mutates the interface", () => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "congested_wifi",
      BOT_MAX_JOIN_STAGGER_SECS: "5m",
    });
    expect(status).toBe(1);
    expect(stderr).toContain("BOT_MAX_JOIN_STAGGER_SECS");
    expect(commands, "a rejected config must not shape the interface").toEqual([]);
  });

  it("fails a misconfigured pod BEFORE sleeping, so the failure is not delayed", () => {
    const { status, stdout, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-7", // no provisioned account
      BOT_MAX_JOIN_STAGGER_SECS: "300",
    });
    expect(status).toBe(1);
    expect(stderr).toContain("ordinal 7 has no provisioned account");
    expect(slept(commands)).toEqual([]);
    expect(stdout).not.toContain("join stagger");
  });

  it("fails an unshapeable pod BEFORE sleeping too, not one stagger later", () => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "bogus",
      BOT_MAX_JOIN_STAGGER_SECS: "300",
    });
    expect(status).toBe(1);
    expect(stderr).toContain("unknown BOT_NETEM_PROFILE");
    expect(slept(commands)).toEqual([]);
  });

  it("records the applied shaping before the stagger, not only at launch", () => {
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_NETEM_PROFILE: "satellite",
      BOT_MAX_JOIN_STAGGER_SECS: "300",
    });
    expect(status).toBe(0);
    expect(stdout).toMatch(/netem applied — shape profile=satellite/);
    expect(stdout.indexOf("netem applied")).toBeLessThan(stdout.indexOf("join stagger"));
  });

  it("lets one ordinal carry a different simulcast cap than the fleet", () => {
    const fleet = { ...ORDINAL_ENV, BOT_HW_CONCURRENCY: "10", BOT_HW_CONCURRENCY_1: "6" };
    const overridden = runEntrypoint({ ...fleet, HOSTNAME: "videocall-bots-1" });
    expect(overridden.status).toBe(0);
    expect(flagValue(overridden.stdout, "--hardware-concurrency")).toBe("6");
    // Ordinal 0 keeps the fleet-wide value: the indirection, not a blanket override.
    const untouched = runEntrypoint({ ...fleet, HOSTNAME: "videocall-bots-0" });
    expect(untouched.status).toBe(0);
    expect(flagValue(untouched.stdout, "--hardware-concurrency")).toBe("10");
  });

  it("resolves ordinal 0's OWN cap, so a single-pod repro is not left uncapped", () => {
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_HW_CONCURRENCY: "10",
      BOT_HW_CONCURRENCY_0: "6",
    });
    expect(status).toBe(0);
    expect(flagValue(stdout, "--hardware-concurrency")).toBe("6");
  });

  it("treats an explicitly EMPTY per-ordinal cap as 'omit the flag for this pod'", () => {
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_HW_CONCURRENCY: "10",
      BOT_HW_CONCURRENCY_1: "",
    });
    expect(status).toBe(0);
    expect(stdout).not.toContain("--hardware-concurrency");
  });

  it("says so when per-ordinal overrides land on a pod that cannot read them", () => {
    const { status, stderr, commands } = runEntrypoint({
      BOT_IDENTITY_MODE: "single",
      BOT_EMAIL: "solo@example.test",
      BOT_PASSWORD: "pw-solo",
      BOT_NETEM_PROFILE_1: "dialup",
      BOT_HW_CONCURRENCY_2: "6",
    });
    expect(status).toBe(0);
    expect(stderr).toContain("ignoring per-ordinal overrides");
    expect(stderr).toContain("BOT_NETEM_PROFILE_1");
    expect(stderr).toContain("BOT_HW_CONCURRENCY_2");
    expect(commands, "an ignored per-ordinal profile must not shape anything").toEqual(probeOnly());
  });

  const SINGLE = { BOT_IDENTITY_MODE: "single", BOT_EMAIL: "s@e.test", BOT_PASSWORD: "pw" };

  it.each([
    ["single mode with nothing injected", SINGLE],
    ["ordinal mode, where they apply", { ...ORDINAL_ENV, BOT_NETEM_PROFILE_1: "dialup" }],
  ])("stays quiet about per-ordinal overrides in %s", (_case, env) => {
    const { status, stderr } = runEntrypoint(env);
    expect(status).toBe(0);
    expect(stderr).not.toContain("per-ordinal overrides");
  });

  const SUFFIX_NOTICE = "take a per-ordinal suffix";
  const UNREAD_SUFFIXED = { BOT_NETEM_IFACE_1: "eth9", BOT_MAX_JOIN_STAGGER_SECS_1: "99" };

  it.each([
    ["ordinal mode", { ...ORDINAL_ENV, HOSTNAME: "videocall-bots-1" }],
    ["single mode", SINGLE],
  ])("names a `_<N>` suffix that no family reads, in %s", (_case, env) => {
    const { status, stdout, stderr, commands } = runEntrypoint({ ...env, ...UNREAD_SUFFIXED });
    expect(status).toBe(0);
    expect(stderr).toContain(SUFFIX_NOTICE);
    expect(stderr).toContain("BOT_NETEM_IFACE_1");
    expect(stderr).toContain("BOT_MAX_JOIN_STAGGER_SECS_1");
    expect(commands, "eth9 was never touched; eth0 was still probed").toEqual(probeOnly());
    expect(stdout).toMatch(/join_stagger=off /m);
  });

  it("stays quiet about the families that DO take a suffix", () => {
    const { status, stderr } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_NETEM_PROFILE_1: "clean",
      BOT_HW_CONCURRENCY_1: "6",
    });
    expect(status).toBe(0);
    expect(stderr).not.toContain(SUFFIX_NOTICE);
  });

  // ORDINAL is `${POD_NAME##*-}`, so only a plain decimal ever reaches a pod.
  const UNREACHABLE_NOTICE = "a per-ordinal suffix must be a plain integer";

  it.each([
    ["a non-numeric suffix", "BOT_NETEM_PROFILE_abc"],
    ["a leading zero, which no ordinal spells", "BOT_NETEM_PROFILE_01"],
    ["a digit/letter transposition", "BOT_HW_CONCURRENCY_1O"],
    ["an ordinal with no provisioned account", "BOT_NETEM_PROFILE_5"],
    ["an empty suffix", "BOT_NETEM_PROFILE_"],
  ])("names %s as read by no pod", (_case, name) => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      [name]: "dialup",
    });
    expect(status).toBe(0);
    expect(stderr).toContain(UNREACHABLE_NOTICE);
    expect(stderr).toContain(name);
    expect(commands, "an unreachable per-ordinal profile shaped nothing").toEqual(probeOnly());
  });

  it.each([
    ["this pod's own ordinal", "BOT_NETEM_PROFILE_0", "clean"],
    ["another provisioned ordinal", "BOT_NETEM_PROFILE_1", "clean"],
    ["a per-ordinal cap for a provisioned ordinal", "BOT_HW_CONCURRENCY_1", "6"],
  ])("stays quiet about %s", (_case, name, value) => {
    const { status, stderr } = runEntrypoint({ ...ORDINAL_ENV, [name]: value });
    expect(status).toBe(0);
    expect(stderr).not.toContain(UNREACHABLE_NOTICE);
  });

  it("names an ordinal with an email but no password as read by no pod", () => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_EMAIL_5: "eve@example.test",
      BOT_NETEM_PROFILE_5: "dialup",
    });
    expect(status).toBe(0);
    expect(stderr).toContain(UNREACHABLE_NOTICE);
    expect(stderr).toContain("BOT_NETEM_PROFILE_5");
    expect(commands).toEqual(probeOnly());
  });

  it("warns about a misspelt stem once, not twice", () => {
    const { status, stderr } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_NETEM_PROFILE_TYPO_1: "dialup",
    });
    expect(status).toBe(0);
    expect(stderr).toContain(SUFFIX_NOTICE);
    expect(stderr, "the stem sweep already names it").not.toContain(UNREACHABLE_NOTICE);
  });

  it("still applies this pod's own profile while naming an unreachable sibling", () => {
    const { status, stdout, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      HOSTNAME: "videocall-bots-1",
      BOT_NETEM_PROFILE_1: "good_4g",
      BOT_NETEM_PROFILE_01: "dialup",
    });
    expect(status).toBe(0);
    expect(stderr).toContain("BOT_NETEM_PROFILE_01");
    expect(stdout).toContain("profile=good_4g");
    expect(commands).toEqual(expectedShape(NETEM_PROFILES.good_4g!));
  });

  it("says so in the README, which is where an operator picks the suffix", () => {
    const readme = readFileSync(fileURLToPath(new URL("../README.md", import.meta.url)), "utf8");
    expect(readme).toContain(
      "In ordinal mode it also warns when the suffix is not a plain integer",
    );
  });

  // A misspelt family shares the real one's prefix, so a prefix allowlist
  // suppresses the warning for the value most likely to be a typo.
  it.each(["BOT_NETEM_PROFILE_TYPO_1", "BOT_HW_CONCURRENCY_X_2", "BOT_EMAIL_LIST_3"])(
    "names %s, whose stem no family reads",
    (name) => {
      const { status, stderr, commands } = runEntrypoint({
        ...ORDINAL_ENV,
        HOSTNAME: "videocall-bots-1",
        [name]: "congested_wifi",
      });
      expect(status).toBe(0);
      expect(stderr).toContain(SUFFIX_NOTICE);
      expect(stderr).toContain(name);
      expect(commands, "the misspelt value shaped nothing").toEqual(probeOnly());
    },
  );

  const FORGED = "netem shape — forged";

  // Both terminators: a surviving lone CR is still a line break to a consumer.
  const CTRL = [
    ["LF", "\n"],
    ["CR", "\r"],
  ] as const;

  // Values that still reach a writer with the control character intact.
  const SINKS = [
    [
      "stdout",
      0,
      "docker-entrypoint: launching bot",
      (v: string) => ({ BOT_IDENTITY_MODE: `single${v}` }),
    ],
    [
      "stderr",
      1,
      "docker-entrypoint: FATAL",
      (v: string) => ({ BOT_MAX_JOIN_STAGGER_SECS: `5${v}` }),
    ],
    ["stderr", 1, "docker-entrypoint: FATAL", (v: string) => ({ BOT_NETEM_PROFILE: `bogus${v}` })],
  ] as const;

  it.each(
    CTRL.flatMap(([label, ctrl]) =>
      SINKS.map(
        ([stream, code, prefix, vars]) =>
          [label, stream, code, prefix, vars(`${ctrl}docker-entrypoint: ${FORGED}`)] as const,
      ),
    ),
  )(
    "keeps a %s in an operator value from forging a second %s line",
    (_label, _stream, code, prefix, vars) => {
      const run = runEntrypoint({
        BOT_IDENTITY_MODE: "single",
        BOT_EMAIL: "solo@example.test",
        BOT_PASSWORD: "pw-solo",
        ...vars,
      });
      expect(run.status).toBe(code);
      // Split on either terminator, or an unstripped CR reads as one long line.
      const lines = `${run.stdout}\n${run.stderr}`.split(/[\r\n]/);
      const own = lines.filter((l) => l.startsWith(prefix));
      expect(own, `exactly one '${prefix}' line`).toHaveLength(1);
      // Truncated here ⇒ the rest of the value became its own unprefixed line.
      expect(own[0]).toContain(FORGED);
      // The acceptance criterion itself: no writer started a NEW prefixed line.
      expect(lines.filter((l) => l.startsWith(`docker-entrypoint: ${FORGED}`))).toEqual([]);
    },
  );
});

describe("docker-entrypoint.sh — CR/LF rejected at the operator boundary (#2375)", () => {
  const FORGED = "boundary — forged";
  const BASE = {
    BOT_IDENTITY_MODE: "single",
    BOT_EMAIL: "solo@example.test",
    BOT_PASSWORD: "pw-solo",
  };

  // Downstream orchestrator.ts / form-login.ts / meeting-join.ts /
  // resource/session.ts / posture.ts interpolate these without collapsing.
  const GATED: Array<[string, (v: string) => Record<string, string>]> = [
    ["MEETING_URL", (v) => ({ MEETING_URL: `https://app.test/meeting/room${v}` })],
    ["BOT_PARTICIPANT", (v) => ({ BOT_PARTICIPANT: `bot-7${v}` })],
    ["TTL", (v) => ({ TTL: `infinite${v}` })],
    ["BOT_AUTH", (v) => ({ BOT_AUTH: `none${v}`, BOT_EMAIL: "", BOT_PASSWORD: "" })],
    ["BOT_RUN_DIR", (v) => ({ BOT_RUN_DIR: `/tmp/bots-run${v}` })],
    ["BOT_HW_CONCURRENCY", (v) => ({ BOT_HW_CONCURRENCY: `10${v}` })],
    ["BOT_INDEX", (v) => ({ BOT_INDEX: `3${v}` })],
    ["BOT_CTL_PORT", (v) => ({ BOT_CTL_PORT: `8080${v}`, BOT_CTL_TOKEN: "t" })],
    [
      "BOT_CTL_BIND",
      (v) => ({ BOT_CTL_PORT: "8080", BOT_CTL_TOKEN: "t", BOT_CTL_BIND: `0.0.0.0${v}` }),
    ],
    ["BOT_CTL_STATE_DIR", (v) => ({ BOT_CTL_STATE_DIR: `/var/lib/bots-ctl${v}` })],
    ["BOT_EMAIL", (v) => ({ BOT_EMAIL: `solo@example.test${v}` })],
    ["BOT_EXTRA_ARGS", (v) => ({ BOT_EXTRA_ARGS: `--sso-state-file /tmp/s.json${v}` })],
  ];

  it("gates exactly the variables this suite and the README enumerate", () => {
    const loop = /\nfor raw_var in ([\s\S]*?); do\n/.exec(readFileSync(ENTRYPOINT, "utf8"));
    if (loop === null) {
      throw new Error("docker-entrypoint.sh no longer loops `for raw_var in …` — update this lock");
    }
    const inScript = loop[1].replace(/\\\n/g, " ").trim().split(/\s+/);
    expect([...inScript].sort()).toEqual(GATED.map(([n]) => n).sort());

    const paragraph = readFileSync(new URL("../README.md", import.meta.url), "utf8")
      .split("\n")
      .find((l) => l.startsWith("A carriage return or newline in any env value"));
    expect(paragraph, "README no longer documents the boundary rejection").toBeDefined();
    const documented = [...(paragraph ?? "").matchAll(/`([A-Z][A-Z0-9_]+)`/g)].map((m) => m[1]);
    expect([...documented].sort()).toEqual([...inScript].sort());
  });

  it.each(
    [
      ["LF", "\n"],
      ["CR", "\r"],
    ].flatMap(([ctrlName, ctrl]) =>
      GATED.map(([name, vars]) => [name, ctrlName, vars(`${ctrl}${FORGED}`)] as const),
    ),
  )("refuses a %s carrying a %s before anything downstream sees it", (name, _ctrl, vars) => {
    const run = runEntrypoint({ ...BASE, ...vars });
    expect(run.status).toBe(1);
    const lines = `${run.stdout}\n${run.stderr}`.split(/[\r\n]/).filter(Boolean);
    const fatal = lines.filter((l) => l.startsWith("docker-entrypoint: FATAL"));
    expect(fatal, `one FATAL naming ${name}`).toHaveLength(1);
    expect(fatal[0]).toContain(name);
    expect(lines.filter((l) => l.includes(FORGED))).toEqual([]);
    // Never reached `exec`, so no downstream writer received the value.
    expect(run.argv).toEqual([]);
  });

  it("names the first gated variable when several carry a control character", () => {
    const run = runEntrypoint({
      ...BASE,
      MEETING_URL: `https://app.test/meeting/room\n${FORGED}`,
      BOT_PARTICIPANT: `bot-7\n${FORGED}`,
    });
    expect(run.status).toBe(1);
    expect(run.stderr).toContain("MEETING_URL");
    expect(run.stderr).not.toContain("BOT_PARTICIPANT");
  });

  it("leaves a clean fleet value untouched on argv", () => {
    const run = runEntrypoint({ ...BASE, BOT_PARTICIPANT: "bot-7", TTL: "30m" });
    expect(run.status).toBe(0);
    expect(flagValue(run.argv, "--participant")).toBe("bot-7");
    expect(flagValue(run.argv, "--ttl")).toBe("30m");
  });
});

describe("docker-entrypoint.sh → resource/session.ts [resource] writer (#2375)", () => {
  const FORGED = "[resource] downstream — forged";

  /** The PRODUCTION `[resource]` writer on the exact `--assets-dir` bytes the
   *  entrypoint passed. */
  function resourceLines(assetsDir: string | undefined): string[] {
    if (assetsDir === undefined) return [];
    const seen: string[] = [];
    const sink = (...a: unknown[]): void => void seen.push(a.map(String).join(" "));
    const log = vi.spyOn(console, "log").mockImplementation(sink);
    const warn = vi.spyOn(console, "warn").mockImplementation(sink);
    try {
      new ResourceCaptureSession({
        runDir: assetsDir,
        label: "forge-probe",
        spawn: (() => Object.assign(new EventEmitter(), { unref: vi.fn(), pid: 99 })) as never,
      }).startLocal();
    } finally {
      log.mockRestore();
      warn.mockRestore();
    }
    return seen.flatMap((l) => l.split(/[\r\n]/)).filter((l) => l.startsWith("[resource]"));
  }

  it("emits exactly one [resource] line for a clean BOT_RUN_DIR", () => {
    const runDir = mkdtempSync(join(tmpdir(), "bot-assets-"));
    try {
      const run = runEntrypoint({
        BOT_IDENTITY_MODE: "single",
        BOT_EMAIL: "solo@example.test",
        BOT_PASSWORD: "pw-solo",
        BOT_RUN_DIR: runDir,
      });
      expect(run.status).toBe(0);
      const assetsDir = flagValue(run.argv, "--assets-dir");
      expect(assetsDir).toBe(runDir);
      const lines = resourceLines(assetsDir);
      expect(lines).toHaveLength(1);
      expect(lines[0]).toContain(runDir);
    } finally {
      rmSync(runDir, { recursive: true, force: true });
    }
  });

  it("collapses a forged value handed over ungated onto the one [resource] line", () => {
    const parent = mkdtempSync(join(tmpdir(), "bot-assets-"));
    try {
      const lines = resourceLines(join(parent, `run\r${FORGED}`));
      expect(lines).toHaveLength(1);
      expect(lines[0]).toContain(FORGED);
    } finally {
      rmSync(parent, { recursive: true, force: true });
    }
  });

  it.each([
    ["LF", "\n"],
    ["CR", "\r"],
  ])("a %s in BOT_RUN_DIR never reaches that writer", (_name, ctrl) => {
    const parent = mkdtempSync(join(tmpdir(), "bot-assets-"));
    try {
      const run = runEntrypoint({
        BOT_IDENTITY_MODE: "single",
        BOT_EMAIL: "solo@example.test",
        BOT_PASSWORD: "pw-solo",
        BOT_RUN_DIR: join(parent, `run${ctrl}${FORGED}`),
      });
      expect(resourceLines(flagValue(run.argv, "--assets-dir"))).toEqual([]);
      expect(run.status).toBe(1);
    } finally {
      rmSync(parent, { recursive: true, force: true });
    }
  });
});

describe("docker-entrypoint.sh → orchestrator.ts [label] writers (#2375)", () => {
  const FORGED = "[bot-7] FORGED-BY-ORCHESTRATOR";
  const BASE = {
    BOT_IDENTITY_MODE: "single",
    BOT_EMAIL: "solo@example.test",
    BOT_PASSWORD: "pw-solo",
  };

  /** The PRODUCTION `[label]` writers in orchestrator.ts on the exact
   *  `--participant` bytes the entrypoint passed, only `launchBot` stubbed. */
  async function orchestratorLines(participant: string | undefined): Promise<string[]> {
    if (participant === undefined) return [];
    vi.resetModules();
    vi.doMock("./bot", async (orig: () => Promise<typeof import("./bot")>) => ({
      ...(await orig()),
      launchBot: vi.fn(async () => ({
        userHangupDetected: new Promise<void>(() => {}),
        leaveMeeting: vi.fn(async () => {}),
        shutdown: vi.fn(async () => {}),
      })),
    }));
    const { runBotsToCompletion } = await import("./orchestrator");
    const { SD_SOURCE } = await import("./posture");
    const seen: string[] = [];
    const sink = (...a: unknown[]): void => void seen.push(a.map(String).join(" "));
    const spies = [
      vi.spyOn(console, "log").mockImplementation(sink),
      vi.spyOn(console, "warn").mockImplementation(sink),
      vi.spyOn(console, "error").mockImplementation(sink),
    ];
    try {
      await runBotsToCompletion({
        tasks: [
          {
            botId: "00000000-0000-0000-0000-000000000001",
            meetingURL: "https://example.test/meeting/X",
            participant,
            displayName: participant,
            headless: true,
            authBackend: "none",
            videoMode: "clock",
            sourceGeometry: SD_SOURCE,
            cameraCycle: null,
            ttl: 10,
          },
        ],
      });
    } finally {
      for (const s of spies) s.mockRestore();
      vi.doUnmock("./bot");
      vi.resetModules();
    }
    return seen.flatMap((l) => l.split(/[\r\n]/)).filter(Boolean);
  }

  it("emits its [label] lines for a clean BOT_PARTICIPANT", async () => {
    const run = runEntrypoint({ ...BASE, BOT_PARTICIPANT: "bot-7" });
    expect(run.status).toBe(0);
    const lines = await orchestratorLines(flagValue(run.argv, "--participant"));
    expect(lines.filter((l) => l.startsWith("[bot-7@")).length).toBeGreaterThan(0);
  });

  it("the probe DOES see a forged line when the value is handed over ungated", async () => {
    const lines = await orchestratorLines(`bot-7\r${FORGED}`);
    expect(lines.filter((l) => l.startsWith(FORGED)).length).toBeGreaterThan(0);
  });

  it.each([
    ["LF", "\n"],
    ["CR", "\r"],
  ])("a %s in BOT_PARTICIPANT never reaches those writers", async (_name, ctrl) => {
    const run = runEntrypoint({ ...BASE, BOT_PARTICIPANT: `bot-7${ctrl}${FORGED}` });
    const lines = await orchestratorLines(flagValue(run.argv, "--participant"));
    expect(lines.filter((l) => l.startsWith(FORGED))).toEqual([]);
    expect(run.status).toBe(1);
  });
});

describe("docker-entrypoint.sh — camera duty cycle (#2362)", () => {
  const ORDINAL_ENV = {
    BOT_IDENTITY_MODE: "ordinal",
    BOT_AUTH: "form-login",
    HOSTNAME: "videocall-bots-0",
    BOT_EMAIL_0: "alice@example.test",
    BOT_PASSWORD_0: "pw-alice",
  };
  const CYCLE = {
    BOT_CAMERA_ON_SECS_MIN: "5",
    BOT_CAMERA_ON_SECS_MAX: "15",
    BOT_CAMERA_OFF_SECS_MIN: "20",
    BOT_CAMERA_OFF_SECS_MAX: "60",
  };

  it("reports 'off' on the launch line when all four are unset", () => {
    const { status, stdout } = runEntrypoint(ORDINAL_ENV);
    expect(status).toBe(0);
    expect(stdout).toMatch(/camera_cycle=\[off\]$/m);
    expect(stdout).toContain("STUB_CAMERA=<unset>|<unset>|<unset>|<unset>");
  });

  it("reports the configured cycle and hands all four to the bot process", () => {
    const { status, stdout } = runEntrypoint({ ...ORDINAL_ENV, ...CYCLE });
    expect(status).toBe(0);
    // "configured", never "applied" — the entrypoint cannot observe a toggle.
    expect(stdout).toMatch(
      /camera_cycle=\[configured on=\[5-15\]s off=\[20-60\]s target_duty=20%\]$/m,
    );
    expect(stdout).toContain("STUB_CAMERA=5|15|20|60");
  });

  // The receipt the shell prints and the one camera-cycle.ts prints must agree,
  // or one of the two is lying about the same configuration.
  it.each([
    ["5", "15", "20", "60"],
    ["10", "10", "30", "30"],
    ["1", "2", "2", "3"],
    ["7", "9", "1", "1"],
  ])("states the same target duty as targetDutyPct for %s-%s / %s-%s", (a, b, c, d) => {
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_CAMERA_ON_SECS_MIN: a,
      BOT_CAMERA_ON_SECS_MAX: b,
      BOT_CAMERA_OFF_SECS_MIN: c,
      BOT_CAMERA_OFF_SECS_MAX: d,
    });
    expect(status).toBe(0);
    const resolved = resolveCameraCycle({ onMin: a, onMax: b, offMin: c, offMax: d });
    expect(resolved.kind).toBe("ok");
    if (resolved.kind !== "ok" || resolved.value === undefined) throw new Error("unreachable");
    expect(stdout).toContain(`target_duty=${targetDutyPct(resolved.value)}%`);
  });

  it.each(CAMERA_CYCLE_ENV_NAMES)("rejects a partial set — only %s given", (name) => {
    const { status, stderr, stdout } = runEntrypoint({ ...ORDINAL_ENV, [name]: "10" });
    expect(status).toBe(1);
    expect(stderr).toContain("camera cycling needs all four");
    // Every one of the other three must be named as missing.
    for (const other of CAMERA_CYCLE_ENV_NAMES.filter((n) => n !== name)) {
      expect(stderr).toContain(other);
    }
    expect(stdout).not.toContain("launching bot");
  });

  it.each([
    ["abc", "at most 5 digits"],
    ["-1", "at most 5 digits"],
    ["1.5", "at most 5 digits"],
    ["10s", "at most 5 digits"],
    ["000000", "at most 5 digits"],
    ["0", ">= 1 second"],
    [String(CAMERA_CYCLE_SECS_CEILING + 1), `<= ${CAMERA_CYCLE_SECS_CEILING}`],
  ])("rejects BOT_CAMERA_OFF_SECS_MAX=%j rather than running always-on", (value, reason) => {
    const { status, stderr } = runEntrypoint({
      ...ORDINAL_ENV,
      ...CYCLE,
      BOT_CAMERA_OFF_SECS_MAX: value,
    });
    expect(status).toBe(1);
    expect(stderr).toContain("BOT_CAMERA_OFF_SECS_MAX");
    expect(stderr).toContain(reason);
  });

  it("accepts the ceiling itself, and a leading zero as base 10", () => {
    const { status, stdout } = runEntrypoint({
      ...ORDINAL_ENV,
      BOT_CAMERA_ON_SECS_MIN: "08",
      BOT_CAMERA_ON_SECS_MAX: String(CAMERA_CYCLE_SECS_CEILING),
      BOT_CAMERA_OFF_SECS_MIN: "09",
      BOT_CAMERA_OFF_SECS_MAX: "10",
    });
    expect(status).toBe(0);
    expect(stdout).toContain(`on=[8-${CAMERA_CYCLE_SECS_CEILING}]s off=[9-10]s`);
  });

  it.each([
    ["BOT_CAMERA_ON_SECS_MIN", "30", "BOT_CAMERA_ON_SECS_MAX"],
    ["BOT_CAMERA_OFF_SECS_MIN", "99", "BOT_CAMERA_OFF_SECS_MAX"],
  ])("rejects %s=%s because it exceeds %s", (name, value, other) => {
    const { status, stderr } = runEntrypoint({ ...ORDINAL_ENV, ...CYCLE, [name]: value });
    expect(status).toBe(1);
    expect(stderr).toContain(name);
    expect(stderr).toContain(other);
  });

  it("rejects a bad cycle BEFORE tc mutates the interface, and before the stagger", () => {
    const { status, stderr, commands } = runEntrypoint({
      ...ORDINAL_ENV,
      ...CYCLE,
      BOT_CAMERA_ON_SECS_MIN: "0",
      BOT_NETEM_PROFILE: "congested_wifi",
      BOT_MAX_JOIN_STAGGER_SECS: "300",
    });
    expect(status).toBe(1);
    expect(stderr).toContain("BOT_CAMERA_ON_SECS_MIN");
    expect(commands, "a rejected cycle must not shape the interface or sleep").toEqual([]);
  });

  it("keeps a CR in a rejected value from forging a second log line", () => {
    const { status, stdout, stderr } = runEntrypoint({
      ...ORDINAL_ENV,
      ...CYCLE,
      BOT_CAMERA_ON_SECS_MAX: "9\rdocker-entrypoint: forged",
    });
    expect(status).toBe(1);
    const lines = `${stdout}\n${stderr}`.split(/[\r\n]/);
    expect(lines.filter((l) => l.startsWith("docker-entrypoint: forged"))).toEqual([]);
  });
});

describe("docker-entrypoint.sh ↔ the image's capability grants (#2353, #2428)", () => {
  const DOCKERFILE = fileURLToPath(new URL("../Dockerfile", import.meta.url));
  const BUILD_SH = fileURLToPath(new URL("../build.sh", import.meta.url));

  /** The `for b in <paths>; do` list a capability loop iterates. */
  const capBinaries = (path: string): string[] => {
    const m = /for b in ([^;]+); do/.exec(readFileSync(path, "utf8"));
    expect(m, `${path} must carry a capability loop`).not.toBeNull();
    return m![1].trim().split(/\s+/);
  };

  const basenames = (paths: string[]): string[] => paths.map((x) => x.replace(/.*\//, "")).sort();

  const setprivCopy = (): { source: string; dest: string } => {
    const m = /cp "\$\(readlink -f (\S+)\)" (\S+)/.exec(readFileSync(DOCKERFILE, "utf8"));
    expect(m, "the Dockerfile must copy setpriv to a dedicated path (#2428)").not.toBeNull();
    return { source: m![1], dest: m![2] };
  };

  const entrypointSetpriv = (): string => {
    const m = /^NETEM_SETPRIV="\$\{NETEM_SETPRIV:-(\S+)\}"$/m.exec(
      readFileSync(ENTRYPOINT, "utf8"),
    );
    expect(m, "the entrypoint must route ip through NETEM_SETPRIV").not.toBeNull();
    return m![1];
  };

  it("caps the tc it calls, the ip it calls, and the setpriv that raises net_admin", () => {
    const code = readFileSync(ENTRYPOINT, "utf8").replace(/^\s*#.*$/gm, "");
    const { source, dest } = setprivCopy();
    expect(entrypointSetpriv(), "NETEM_SETPRIV must be the Dockerfile's copy").toBe(dest);
    expect(basenames([source]), "the wrapper must be setpriv").toEqual(["setpriv"]);
    // iproute2 clears its own caps unless net_admin is INHERITABLE, so both flags bind.
    expect(code).toMatch(
      /"\$\{NETEM_SETPRIV\}" --inh-caps \+net_admin --ambient-caps \+net_admin -- ip "\$@"/,
    );
    expect(code).not.toMatch(/(^|[\s|(])ip (link|qdisc|filter) /m);
    expect(code, "the shaping path must still call tc").toMatch(/(^|[\s|(])tc (qdisc|filter) /m);

    const caps = capBinaries(DOCKERFILE);
    expect(caps, "the wrapper is what raises the inheritable cap").toContain(dest);
    expect(
      basenames(caps.filter((p) => p !== dest)),
      "a cap on a binary nothing calls is privilege for nothing",
    ).toEqual(["ip", "tc"]);
  });

  it("resolves each target through readlink and re-reads the cap it just set", () => {
    const df = readFileSync(DOCKERFILE, "utf8");
    // setcap refuses a symlink, and /usr/sbin/ip is one in the base image.
    expect(df).toMatch(/p="\$\(readlink -f "\$b"\)"/);
    // `|| exit 1` on BOTH: a bare loop returns only its last iteration's status.
    expect(df).toMatch(/setcap cap_net_admin\+\w+ "\$p" \|\| exit 1/);
    expect(df).toMatch(/getcap "\$p" \| grep -q cap_net_admin \|\| exit 1/);
    expect(
      df.indexOf(setprivCopy().dest),
      "the copy must exist before the loop setcaps it",
    ).toBeLessThan(df.indexOf("for b in "));
  });

  // The Dockerfile's own re-read runs in the same layer as the setcap, so it
  // cannot see a backend dropping the xattr at layer-commit; build.sh's can.
  it("re-checks the same binaries after the build, and fails rather than warns", () => {
    expect(capBinaries(BUILD_SH)).toEqual(capBinaries(DOCKERFILE));
    const sh = readFileSync(BUILD_SH, "utf8");
    expect(sh).toContain('p="$(readlink -f "$b")"');
    expect(sh).toMatch(/getcap "\$p" \| grep -q cap_net_admin \|\| \{[^}]*exit 1; \}/);
    expect(sh, "a warn-only guard ships a green build with no caps").toMatch(
      /Refusing to tag this build/,
    );
  });
});
