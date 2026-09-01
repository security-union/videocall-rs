import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

// Fleet resource sizing spans three manifests; a half-applied raise is
// otherwise silent until the next run (#2313).
const K8S = resolve(import.meta.dirname, "..", "k8s");
const readDocs = (name: string): unknown[] =>
  readFileSync(resolve(K8S, `${name}.yaml`), "utf8")
    .split(/^---$/m)
    .map((d) => parseYaml(d))
    .filter((d): d is object => d !== null && typeof d === "object");

/** The manifest's own stated sizing target; the quota must admit this many. */
const FLEET_TARGET = 20;

const MEASURED_PEAK_WORKING_SET_MIB = 1736;

interface Resources {
  requests: { cpu: string; memory: string };
  limits: { cpu: string; memory: string };
}
interface Container {
  name?: string;
  resources?: Resources;
  volumeMounts?: { name: string; mountPath: string }[];
}
interface PodSpec {
  containers: Container[];
  volumes?: { name: string; emptyDir?: { sizeLimit?: string; medium?: string } }[];
}

/** Kubernetes cpu quantity -> millicores. */
const cpuMilli = (q: string): number =>
  q.endsWith("m") ? Number(q.slice(0, -1)) : Math.round(Number(q) * 1000);

/** Kubernetes memory quantity -> mebibytes. */
const memMib = (q: string): number => {
  const m = /^(\d+(?:\.\d+)?)(Ki|Mi|Gi|Ti)?$/.exec(q);
  if (!m) throw new Error(`unparseable memory quantity: ${q}`);
  const scale = { Ki: 1 / 1024, Mi: 1, Gi: 1024, Ti: 1024 * 1024 }[m[2] ?? "Mi"] ?? 1;
  return Number(m[1]) * scale;
};

const botSpec = (): PodSpec => {
  const sts = readDocs("statefulset").find(
    (d) => (d as { kind?: string }).kind === "StatefulSet",
  ) as { spec: { template: { spec: PodSpec } } } | undefined;
  expect(sts, "statefulset.yaml has no StatefulSet document").toBeDefined();
  return sts!.spec.template.spec;
};

const standaloneSpec = (): PodSpec => {
  const pod = readDocs("bot-pod").find((d) => (d as { kind?: string }).kind === "Pod") as
    | { spec: PodSpec }
    | undefined;
  expect(pod, "bot-pod.yaml has no Pod document").toBeDefined();
  return pod!.spec;
};

const botContainer = (spec: PodSpec): Container => {
  const c = spec.containers[0];
  expect(c?.resources, "the bot container declares no resources").toBeDefined();
  return c;
};

const quota = (): Record<string, string> => {
  const q = readDocs("namespace").find((d) => (d as { kind?: string }).kind === "ResourceQuota") as
    | { spec: { hard: Record<string, string> } }
    | undefined;
  expect(q, "namespace.yaml has no ResourceQuota document").toBeDefined();
  return q!.spec.hard;
};

describe("bot fleet resource sizing", () => {
  // Hand-copied blocks: raising one alone makes a standalone bot and a fleet
  // bot measure different workloads.
  it("statefulset.yaml and bot-pod.yaml declare identical resources", () => {
    expect(botContainer(standaloneSpec()).resources).toEqual(botContainer(botSpec()).resources);
  });

  it.each([
    ["limits.cpu", (r: Resources) => cpuMilli(r.limits.cpu), cpuMilli],
    ["requests.cpu", (r: Resources) => cpuMilli(r.requests.cpu), cpuMilli],
    ["limits.memory", (r: Resources) => memMib(r.limits.memory), memMib],
    ["requests.memory", (r: Resources) => memMib(r.requests.memory), memMib],
  ])("the quota's %s admits the stated fleet target", (key, perPod, parse) => {
    const res = botContainer(botSpec()).resources!;
    const admits = Math.floor(parse(quota()[key]) / perPod(res));
    expect(
      admits,
      `quota ${key}=${quota()[key]} admits ${admits} pods, below the ${FLEET_TARGET}-bot target`,
    ).toBeGreaterThanOrEqual(FLEET_TARGET);
  });

  it("requests.memory covers the measured per-pod working set", () => {
    const res = botContainer(botSpec()).resources!;
    expect(
      memMib(res.requests.memory),
      `requests.memory=${res.requests.memory} is under the ${MEASURED_PEAK_WORKING_SET_MIB} MiB peak working set measured by the 8-replica run of 2026-08-22 (#2313); a pod above its own request is what the kubelet ranks on when it evicts`,
    ).toBeGreaterThanOrEqual(MEASURED_PEAK_WORKING_SET_MIB);
  });

  it("the quota admits the fleet target in pod count too", () => {
    expect(Number(quota()["pods"])).toBeGreaterThanOrEqual(FLEET_TARGET);
  });

  // A tmpfs emptyDir is charged to the pod memory limit, so a cap at or above
  // the burst band leaves no OOM margin at the request floor.
  it("the tmpfs /dev/shm cap stays inside the memory burst band", () => {
    const spec = botSpec();
    const res = botContainer(spec).resources!;
    const band = memMib(res.limits.memory) - memMib(res.requests.memory);
    const dshm = spec.volumes?.find((v) => v.name === "dshm");
    expect(dshm?.emptyDir?.medium, "dshm is not a tmpfs emptyDir").toBe("Memory");
    const cap = memMib(dshm!.emptyDir!.sizeLimit!);
    expect(cap, `dshm ${cap}Mi vs burst band ${band}Mi`).toBeLessThan(band);
  });
});
