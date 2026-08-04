import { execFileSync, spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

// The PRODUCTION entrypoint — not a reimplementation. Reverting the ordinal
// resolution in this script makes the assertions below fail.
const ENTRYPOINT = fileURLToPath(new URL("../docker-entrypoint.sh", import.meta.url));

/**
 * Run the entrypoint with a controlled env in a hermetic workdir. A stub `tsx`
 * stands in for `node_modules/.bin/tsx` (the entrypoint `exec`s it as the final
 * step) and prints the credentials it inherited + the args it was handed, so the
 * test can assert what the entrypoint resolved WITHOUT launching a browser.
 *
 * Returns the combined stdout and the exit status. On a non-zero exit
 * `execFileSync` throws; we normalize that into `{ status, stdout, stderr }`.
 */
function runEntrypoint(vars: Record<string, string>): {
  status: number;
  stdout: string;
  stderr: string;
} {
  const workdir = mkdtempSync(join(tmpdir(), "bot-entrypoint-"));
  const binDir = join(workdir, "node_modules", ".bin");
  mkdirSync(binDir, { recursive: true });
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
      'echo "STUB_ARGS=$*"',
      "",
    ].join("\n"),
  );
  chmodSync(stub, 0o755);

  // Replace the env entirely (execFileSync `env` does not merge) but keep PATH so
  // bash/mkdir/coreutils resolve. BOT_RUN_DIR is pointed inside the workdir so
  // the run is hermetic.
  const env: Record<string, string> = {
    PATH: process.env.PATH ?? "/usr/bin:/bin",
    HOME: workdir,
    BOT_RUN_DIR: join(workdir, "run"),
    ...vars,
  };
  try {
    const stdout = execFileSync("bash", [ENTRYPOINT], { cwd: workdir, env, encoding: "utf8" });
    return { status: 0, stdout, stderr: "" };
  } catch (e) {
    const err = e as { status?: number; stdout?: string; stderr?: string };
    return { status: err.status ?? 1, stdout: err.stdout ?? "", stderr: err.stderr ?? "" };
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
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
    PATH: process.env.PATH ?? "/usr/bin:/bin",
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
