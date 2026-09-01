import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { CTL_STATE_DIR_ENV, resolveCtlStateDir } from "./auth";
import { isLoopbackBindAddress } from "./server";

/**
 * Drift lock between `k8s/statefulset.yaml`'s env vars and the volumeMount paths
 * they must match (issues #2157 and #2154).
 *
 * Two contracts in that manifest are load-bearing but were enforced only by
 * ENGLISH COMMENTS ("Must stay in sync with the `ctl-state` volumeMount path"):
 *
 *   1. `BOT_CTL_STATE_DIR` must equal the `ctl-state` emptyDir mountPath. If it
 *      drifts, the cleartext ctl bearer token stops landing on the pod-lifetime
 *      emptyDir. It lands on whatever the path resolves to instead — the
 *      container filesystem, or worse the retained `run-artifacts` PVC — which
 *      is exactly the #2157 defect: a credential that outlives the workload and
 *      survives a rotation of the bot-ctl-token Secret.
 *   2. `BOT_RUN_DIR` must equal the `run-artifacts` PVC mountPath. If it drifts,
 *      the #2032 resource CSVs are written to the container FS and die with the
 *      pod on scale-down — the artifact loss that cost the 2026-07-31 #2143 run
 *      its pod-vs-Prometheus validation.
 *
 * A previous version of this suite "covered" contract 1 with two hardcoded
 * string constants and a comment naming the manifest. That is the `X == X`
 * shape: changing the manifest to a drifted path left every test passing
 * (verified). These tests PARSE the real manifest, so the manifest is the single
 * source of truth and a drifted value fails.
 */

interface EnvVar {
  name: string;
  value?: string;
}
interface VolumeMount {
  name: string;
  mountPath: string;
}
interface ContainerPort {
  name?: string;
  containerPort: number;
}
interface ExecProbe {
  exec?: { command?: string[] };
  httpGet?: unknown;
  initialDelaySeconds?: number;
  periodSeconds?: number;
  timeoutSeconds?: number;
  failureThreshold?: number;
}
interface BotContainer {
  env: EnvVar[];
  volumeMounts: VolumeMount[];
  ports?: ContainerPort[];
  readinessProbe?: ExecProbe;
}

/** Parse the shipped StatefulSet and pull out the bot container. */
function loadBotContainer(): BotContainer {
  // src/control/ -> bots-app/k8s/statefulset.yaml
  const path = resolve(import.meta.dirname, "..", "..", "k8s", "statefulset.yaml");
  const doc = parseYaml(readFileSync(path, "utf8")) as {
    spec: { template: { spec: { containers: BotContainer[] } } };
  };
  const containers = doc.spec.template.spec.containers;
  expect(containers, "statefulset.yaml has no containers").toBeInstanceOf(Array);
  expect(containers.length).toBeGreaterThan(0);
  return containers[0];
}

function envValue(env: EnvVar[], name: string): string {
  const hit = env.find((e) => e.name === name);
  expect(hit, `statefulset.yaml container env is missing ${name}`).toBeDefined();
  const value = hit?.value;
  expect(typeof value, `${name} must be a literal string value`).toBe("string");
  return value as string;
}

function mountPath(mounts: VolumeMount[], name: string): string {
  const hit = mounts.find((m) => m.name === name);
  expect(hit, `statefulset.yaml container has no volumeMount named ${name}`).toBeDefined();
  return (hit as VolumeMount).mountPath;
}

const PROBE_PORT_REF = "${BOT_CTL_PORT:?}";
const PROBE_BINARY = "curl";
const PROBE_CURL_TIMEOUT_SEC = 3;
const PROBE_PATH = "/healthz";
const PROBE_MAX_DETECT_SEC = 60;

/** Pinned whole, not per-token: any respelling of a readiness-critical token lands here. */
const EXPECTED_PROBE_COMMAND = [
  "/bin/sh",
  "-c",
  `${PROBE_BINARY} -fsS -m ${PROBE_CURL_TIMEOUT_SEC} http://127.0.0.1:${PROBE_PORT_REF}${PROBE_PATH}`,
];

/** Packages a non-simulated apt-get install adds in the Dockerfile's FINAL stage. */
function finalStageAptPackages(): string[] {
  const lines = readFileSync(resolve(import.meta.dirname, "..", "..", "Dockerfile"), "utf8").split(
    "\n",
  );
  const lastFrom = lines.reduce((last, l, i) => (/^\s*FROM\s/i.test(l) ? i : last), -1);
  expect(lastFrom, "Dockerfile has no FROM").toBeGreaterThan(-1);
  const stage = lines
    .slice(lastFrom)
    .join("\n")
    .replace(/\\\n/g, " ")
    .split("\n")
    .map((l) => l.replace(/#.*$/, ""))
    .join("\n");
  const pkgs: string[] = [];
  for (const cmd of stage.split(/&&|\|\||[;|\n]/)) {
    const hit = /\bapt(?:-get)?\s+((?:-\S+\s+)*)install\s+([^\n]*)/.exec(cmd);
    if (!hit) continue;
    const flags = `${hit[1]} ${hit[2]}`;
    // -s/--simulate/--dry-run resolve the package and install nothing.
    if (/(^|\s)(-\w*s\w*|--simulate|--dry-run|--just-print|--no-act)(\s|$)/.test(flags)) continue;
    for (const tok of hit[2].trim().split(/\s+/)) {
      if (tok && !tok.startsWith("-")) pkgs.push(tok.replace(/[=/].*$/, ""));
    }
  }
  return pkgs;
}

describe("statefulset.yaml env ↔ volumeMount drift (#2157 / #2154)", () => {
  it("BOT_CTL_STATE_DIR equals the ctl-state emptyDir mountPath", () => {
    const { env, volumeMounts } = loadBotContainer();
    // Both sides read from the parsed manifest — neither is a literal in this
    // file, so a drift in either place fails here.
    expect(envValue(env, CTL_STATE_DIR_ENV)).toBe(mountPath(volumeMounts, "ctl-state"));
  });

  it("BOT_RUN_DIR equals the run-artifacts PVC mountPath", () => {
    const { env, volumeMounts } = loadBotContainer();
    expect(envValue(env, "BOT_RUN_DIR")).toBe(mountPath(volumeMounts, "run-artifacts"));
  });

  it("keeps the ctl token OFF the retained PVC (the actual #2157 invariant)", () => {
    const { env } = loadBotContainer();
    const runDir = envValue(env, "BOT_RUN_DIR");
    const stateDir = envValue(env, CTL_STATE_DIR_ENV);
    // The point of #2157: the two dirs must be DIFFERENT, and the state dir must
    // not be nested inside the PVC (which would put the token back on the
    // retained volume by another route).
    expect(stateDir).not.toBe(runDir);
    expect(stateDir.startsWith(runDir.endsWith("/") ? runDir : `${runDir}/`)).toBe(false);
    // And the production resolver must actually honour the manifest's override —
    // this is what ties the YAML to the code path that consumes it.
    expect(resolveCtlStateDir(runDir, { [CTL_STATE_DIR_ENV]: stateDir })).toBe(stateDir);
  });

  it("mounts ctl-state from a pod-lifetime emptyDir, not a PVC", () => {
    // The whole security property depends on the volume TYPE, not just the path:
    // an emptyDir dies with the pod, a claim does not.
    const path = resolve(import.meta.dirname, "..", "..", "k8s", "statefulset.yaml");
    const doc = parseYaml(readFileSync(path, "utf8")) as {
      spec: {
        template: { spec: { volumes: Array<Record<string, unknown>> } };
        volumeClaimTemplates?: Array<{ metadata: { name: string } }>;
      };
    };
    const vol = doc.spec.template.spec.volumes.find((v) => v.name === "ctl-state");
    expect(vol, "no `ctl-state` volume in statefulset.yaml").toBeDefined();
    expect(vol, "`ctl-state` must be an emptyDir (pod lifetime)").toHaveProperty("emptyDir");
    // ...and must NOT have been promoted to a claim template.
    const claims = (doc.spec.volumeClaimTemplates ?? []).map((c) => c.metadata.name);
    expect(claims).not.toContain("ctl-state");
    // run-artifacts, by contrast, SHOULD be a claim — that persistence is
    // deliberate (#2032) and this fix must not regress it.
    expect(claims).toContain("run-artifacts");
  });
});

describe("statefulset.yaml readinessProbe ↔ the control server it probes (#2349)", () => {
  it("execs exactly the argv this suite pins", () => {
    const { readinessProbe } = loadBotContainer();
    expect(readinessProbe, "the bot container has no readinessProbe").toBeDefined();
    expect(readinessProbe?.httpGet, "an httpGet reply leaves via eth0, which /netem shapes").toBe(
      undefined,
    );
    expect(readinessProbe?.exec?.command).toEqual(EXPECTED_PROBE_COMMAND);
  });

  it("execs a binary the Dockerfile's final stage installs", () => {
    // Guards the Dockerfile source, not the deployed image. The base image
    // (mcr.microsoft.com/playwright:v1.58.2-noble) also ships curl at
    // /usr/bin/curl — verified 2026-08-20 via `docker run`.
    expect(
      finalStageAptPackages(),
      `the probe execs \`${PROBE_BINARY}\`, which no non-simulated apt-get install in the Dockerfile's final stage adds`,
    ).toContain(PROBE_BINARY);
  });

  it("bounds the fetch inside the kubelet timeout, which is inside the period", () => {
    const { readinessProbe } = loadBotContainer();
    const timeout = readinessProbe?.timeoutSeconds;
    const period = readinessProbe?.periodSeconds;
    expect(timeout, "readinessProbe has no explicit timeoutSeconds (defaults to 1)").toBeDefined();
    expect(period, "readinessProbe has no explicit periodSeconds").toBeDefined();
    expect(PROBE_CURL_TIMEOUT_SEC, "curl -m must end the attempt before kubelet does").toBeLessThan(
      timeout as number,
    );
    expect(
      timeout as number,
      "timeoutSeconds must be below periodSeconds, or a slow probe runs at a 100% duty cycle",
    ).toBeLessThan(period as number);
  });

  it("cannot be tuned into a probe that never gates or never fails", () => {
    const { readinessProbe } = loadBotContainer();
    const delay = readinessProbe?.initialDelaySeconds;
    const threshold = readinessProbe?.failureThreshold;
    const period = readinessProbe?.periodSeconds;
    expect(delay, "readinessProbe has no explicit initialDelaySeconds").toBeDefined();
    expect(threshold, "readinessProbe has no explicit failureThreshold").toBeDefined();
    expect(period, "readinessProbe has no explicit periodSeconds").toBeDefined();
    expect(delay as number).toBeGreaterThan(0);
    expect(delay as number, "a large delay wedges a readiness-gated rollout").toBeLessThanOrEqual(
      60,
    );
    expect(threshold as number).toBeGreaterThan(1);
    expect(
      threshold as number,
      "a large threshold means a dead control server never reaches NotReady",
    ).toBeLessThanOrEqual(10);
    expect(
      (period as number) * (threshold as number),
      "periodSeconds x failureThreshold is how long a dead control server keeps reporting Ready",
    ).toBeLessThanOrEqual(PROBE_MAX_DETECT_SEC);
  });

  it("names a port the container publishes, and a bind the fleet can still reach", () => {
    const { env, ports } = loadBotContainer();
    const ctlPort = Number(envValue(env, "BOT_CTL_PORT"));
    expect(Number.isInteger(ctlPort), "BOT_CTL_PORT is not a port number").toBe(true);
    const ctl = (ports ?? []).find((p) => p.name === "ctl");
    expect(ctl, "statefulset.yaml declares no container port named `ctl`").toBeDefined();
    expect(ctl?.containerPort).toBe(ctlPort);
    expect(
      EXPECTED_PROBE_COMMAND.join(" "),
      "the pinned probe argv must read the port from BOT_CTL_PORT, not a second copy",
    ).toContain(PROBE_PORT_REF);
    expect(
      isLoopbackBindAddress(envValue(env, "BOT_CTL_BIND")),
      "BOT_CTL_BIND must be routable: a loopback bind 501s /netem fleet-wide (cli.ts gates netemEnabled on it) and hides every pod from the conductor, while the probe stays green",
    ).toBe(false);
  });

  it("keeps the conductor addressing pods a failing probe marks NotReady", () => {
    expect(loadBotContainer().readinessProbe).toBeDefined();
    const svc = parseYaml(
      readFileSync(resolve(import.meta.dirname, "..", "..", "k8s", "service.yaml"), "utf8"),
    ) as { spec: { publishNotReadyAddresses?: boolean } };
    expect(
      svc.spec.publishNotReadyAddresses,
      "statefulset.yaml has a readinessProbe, so service.yaml must keep publishNotReadyAddresses: true or a NotReady pod leaves DNS mid-run",
    ).toBe(true);
  });
});

describe("README's opt-in impairment vars ↔ their absence from the manifest (#2359)", () => {
  const readme = (): string =>
    readFileSync(resolve(import.meta.dirname, "..", "..", "README.md"), "utf8");

  /** The section whose opt-in framing rests on these vars being unset by default. */
  function section(): string {
    const m = /\n### Impairment from the moment a bot joins[\s\S]*?(?=\n## )/.exec(readme());
    expect(m, "the README's startup-impairment section must be findable").not.toBeNull();
    return m![0];
  }

  /** First cell of every var row in that section's table. */
  function documentedVars(): string[] {
    const out = [...section().matchAll(/^\| `(BOT_[A-Z_]+(?:_<N>)?)` *\|/gm)].map((m) => m[1]);
    expect(out.length, "the section's var table must parse to at least one row").toBeGreaterThan(0);
    return out;
  }

  /** The absence claim's coverage: the table's rows plus its prose-only var. */
  const claimVars = (): string[] => [...documentedVars(), "BOT_NETEM_IFACE"];

  it("keeps the absence claim, and a table to check it against", () => {
    expect(section()).toContain("none is present in `k8s/statefulset.yaml`");
    expect(documentedVars()).toEqual(
      expect.arrayContaining([
        "BOT_NETEM_PROFILE",
        "BOT_MAX_JOIN_STAGGER_SECS",
        "BOT_HW_CONCURRENCY_<N>",
      ]),
    );
    expect(section(), "BOT_NETEM_IFACE has no table row, so only the prose names it").toContain(
      "`BOT_NETEM_IFACE`",
    );
  });

  it.each(claimVars())("%s is absent from the shipped manifest's env", (name) => {
    const present = loadBotContainer().env.map((e) => e.name);
    const shaped = name.endsWith("_<N>")
      ? present.filter((n) => new RegExp(`^${name.slice(0, -4)}_\\d+$`).test(n))
      : present.filter((n) => n === name);
    expect(
      shaped,
      `the README calls ${name} opt-in and absent; a shaped or staggered default fleet under-represents load on every run`,
    ).toEqual([]);
  });
});

describe("README's fleet-ramp recipe ↔ manifest + log line (#2294)", () => {
  const readme = (): string =>
    readFileSync(resolve(import.meta.dirname, "..", "..", "README.md"), "utf8");

  it("uses the namespace and instance label the StatefulSet actually ships", () => {
    const path = resolve(import.meta.dirname, "..", "..", "k8s", "statefulset.yaml");
    const doc = parseYaml(readFileSync(path, "utf8")) as {
      metadata: { namespace: string };
      spec: { template: { metadata: { labels: Record<string, string> } } };
    };
    const instance = doc.spec.template.metadata.labels["app.kubernetes.io/instance"];
    expect(instance, "pod template has no app.kubernetes.io/instance label").toBeTruthy();
    const cmds = readme()
      .split("\n")
      .filter((l) => l.includes("kubectl") && l.includes("logs -l"));
    expect(
      cmds.length,
      "a log recipe was deleted, or reflowed so `logs -l` is no longer on one line",
    ).toBeGreaterThanOrEqual(2);
    for (const cmd of cmds) {
      expect(cmd).toContain(`-n ${doc.metadata.namespace} `);
      expect(cmd).toContain(`app.kubernetes.io/instance=${instance}`);
      expect(cmd, "a logs -l recipe lost --tail=-1").toContain("--tail=-1");
    }
  });

  it("greps a string the orchestrator still emits on join", () => {
    const src = readFileSync(resolve(import.meta.dirname, "..", "orchestrator.ts"), "utf8");
    expect(src).toContain("] joined; ttl=");
    expect(readme()).toContain('grep "] joined; ttl="');
  });

  it("asks kubectl for timestamps on the recipe whose output is read as a ramp", () => {
    const lines = readme().split("\n");
    const grepAt = lines.findIndex((l) => l.includes('grep "] joined; ttl="'));
    expect(grepAt, "no ramp recipe in the README").toBeGreaterThan(0);
    const kubectlAt = lines
      .slice(0, grepAt)
      .reduce((last, l, i) => (l.includes("kubectl") ? i : last), -1);
    expect(
      lines[kubectlAt] ?? "",
      "the ramp recipe lost --timestamps, or was reflowed so this scan no longer finds it",
    ).toContain("--timestamps");
  });
});
