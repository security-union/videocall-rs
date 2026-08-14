import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { CTL_STATE_DIR_ENV, resolveCtlStateDir } from "./auth";

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

/** Parse the shipped StatefulSet and pull out the bot container's env + mounts. */
function loadBotContainer(): { env: EnvVar[]; volumeMounts: VolumeMount[] } {
  // src/control/ -> bots-app/k8s/statefulset.yaml
  const path = resolve(import.meta.dirname, "..", "..", "k8s", "statefulset.yaml");
  const doc = parseYaml(readFileSync(path, "utf8")) as {
    spec: {
      template: { spec: { containers: Array<{ env: EnvVar[]; volumeMounts: VolumeMount[] }> } };
    };
  };
  const containers = doc.spec.template.spec.containers;
  expect(containers, "statefulset.yaml has no containers").toBeInstanceOf(Array);
  expect(containers.length).toBeGreaterThan(0);
  return { env: containers[0].env, volumeMounts: containers[0].volumeMounts };
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
