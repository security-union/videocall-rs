import { execFileSync, spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { relative, resolve } from "node:path";

import { parseAllDocuments, parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

/** Drift lock over every k8s manifest that runs the bots-app image (#2294). */

const K8S = resolve(import.meta.dirname, "..", "..", "k8s");
const PREPULL_SH = resolve(K8S, "prepull-image.sh");
const REPIN_SH = resolve(K8S, "repin.sh");
const BOT_IMAGE_REPO = "videocall-bots-app";
const TEMPLATE = "image-prepull-job.yaml.tmpl";
const IMAGE_PLACEHOLDER = "__BOT_IMAGE__";
const POLICY_PLACEHOLDER = "__BOT_IMAGE_PULL_POLICY__";
/** A dated tag names the build; the digest is what makes IfNotPresent safe. */
const PINNED_REF = /:\d+\.\d+\.\d+-\d{8}-[0-9a-f]{7,}@sha256:[0-9a-f]{64}$/;

interface Container {
  name: string;
  image?: string;
  imagePullPolicy?: string;
  securityContext?: {
    allowPrivilegeEscalation?: boolean;
    capabilities?: { drop?: string[] };
    seccompProfile?: { type?: string };
  };
}
interface PodSpec {
  containers?: Container[];
  initContainers?: Container[];
  tolerations?: unknown[];
  restartPolicy?: string;
  securityContext?: { runAsNonRoot?: boolean };
  imagePullSecrets?: Array<{ name?: string }>;
  affinity?: {
    podAntiAffinity?: {
      requiredDuringSchedulingIgnoredDuringExecution?: Array<{ topologyKey?: string }>;
    };
  };
}
interface ImageRef {
  file: string;
  container: string;
  image: string;
  policy?: string;
  pullSecrets: string[];
}

/** .yml counts: a manifest outside this glob is outside the invariant. */
function manifestFiles(dir = K8S): string[] {
  return readdirSync(dir)
    .filter((f) => /\.ya?ml$/.test(f))
    .sort();
}

/** Every object carrying a container list, at any depth and in any kind. */
function podSpecs(node: unknown, out: PodSpec[] = []): PodSpec[] {
  if (Array.isArray(node)) {
    for (const child of node) podSpecs(child, out);
    return out;
  }
  if (node !== null && typeof node === "object") {
    const obj = node as Record<string, unknown>;
    if (Array.isArray(obj.containers) || Array.isArray(obj.initContainers)) {
      out.push(obj as PodSpec);
    }
    for (const value of Object.values(obj)) podSpecs(value, out);
  }
  return out;
}

/** Selected by image reference, not filename: another image in k8s/ is not ours. */
function botImageRefs(dir = K8S): ImageRef[] {
  const refs: ImageRef[] = [];
  for (const file of manifestFiles(dir)) {
    for (const doc of parseAllDocuments(readFileSync(resolve(dir, file), "utf8"))) {
      for (const spec of podSpecs(doc.toJS())) {
        for (const c of [...(spec.containers ?? []), ...(spec.initContainers ?? [])]) {
          if (typeof c.image !== "string" || !c.image.includes(BOT_IMAGE_REPO)) continue;
          refs.push({
            file,
            container: c.name,
            image: c.image,
            policy: c.imagePullPolicy,
            pullSecrets: (spec.imagePullSecrets ?? []).map((s) => s.name ?? "").sort(),
          });
        }
      }
    }
  }
  return refs;
}

function agreedImage(): string {
  const images = [...new Set(botImageRefs().map((r) => r.image))];
  expect(images).toHaveLength(1);
  return images[0];
}

function readYaml<T>(name: string): T {
  return parseYaml(readFileSync(resolve(K8S, name), "utf8")) as T;
}

/** Runs the production script, so a broken extraction fails here. */
function renderPrepull(completions: number): {
  spec: {
    completions: number;
    parallelism: number;
    backoffLimit: number;
    activeDeadlineSeconds: number;
    template: { spec: PodSpec };
  };
} {
  const text = execFileSync("bash", [PREPULL_SH, "--render", String(completions)], {
    encoding: "utf8",
  });
  const body = text
    .split("\n")
    .filter((l) => !/^\s*#/.test(l))
    .join("\n");
  expect(body, "rendered Job still contains a placeholder").not.toMatch(/__/);
  return parseYaml(text) as ReturnType<typeof renderPrepull>;
}

/** A copy of k8s/ the test may edit, so a fixture never mutates the repo. */
function manifestCopy(): string {
  const dir = mkdtempSync(resolve(tmpdir(), "prepull-k8s-"));
  for (const f of [...manifestFiles(), TEMPLATE, "prepull-image.sh", "repin.sh"]) {
    copyFileSync(resolve(K8S, f), resolve(dir, f));
  }
  return dir;
}

interface Cluster {
  nodes?: string;
  /** report() rows: nodeName|phase|startTime|terminatedAt|imageID */
  rows?: string;
  /** verify_coverage(): one nodeName per line. */
  pods?: string;
  /** Job conditions, `Type=Status` per line. */
  conds?: string;
  /** pull_trouble() rows: `nodeName <waiting reason>`. */
  waiting?: string;
  /** Make `get job` exit non-zero, as an unreachable apiserver does. */
  jobFails?: boolean;
}
/** `log` is the file every stubbed kubectl invocation appends its args to. */
type Runner = ((...args: string[]) => string) & { log: string; script: string; bin: string };

/**
 * `kubectl` stub on PATH, so the script's own scans and its whole `run`
 * orchestration execute against a fixture. Dispatches on the jsonpath the
 * script passes: report(), verify_coverage() and pull_trouble() all `get pods`,
 * so the narrower patterns must come first.
 */
function withStubbedKubectl(
  cluster: Cluster,
  scriptDir?: string,
  extraEnv: Record<string, string> = {},
): Runner {
  const bin = mkdtempSync(resolve(tmpdir(), "prepull-bin-"));
  const log = resolve(bin, "calls.log");
  writeFileSync(
    resolve(bin, "kubectl"),
    [
      "#!/bin/sh",
      'args="$*"',
      `printf '%s\\n' "$args" >>'${log}'`,
      'case "$args" in',
      "  *'delete job'*) exit 0 ;;",
      "  *'apply -f'*) cat >/dev/null; exit 0 ;;",
      `  *'get nodes'*) printf '%s' '${cluster.nodes ?? ""}'; exit 0 ;;`,
      `  *'get job'*) ${cluster.jobFails ? "exit 1" : `printf '%s' '${cluster.conds ?? "Complete=True\n"}'; exit 0`} ;;`,
      `  *'state.waiting'*) printf '%s' '${cluster.waiting ?? ""}'; exit 0 ;;`,
      `  *containerStatuses*) printf '%s' '${cluster.rows ?? ""}'; exit 0 ;;`,
      `  *'get pods'*) printf '%s' '${cluster.pods ?? ""}'; exit 0 ;;`,
      "esac",
      "exit 0",
    ].join("\n"),
    { mode: 0o755 },
  );
  const script = scriptDir ? resolve(scriptDir, "prepull-image.sh") : PREPULL_SH;
  const run = (...args: string[]) =>
    execFileSync("bash", [script, ...args], {
      encoding: "utf8",
      stdio: "pipe",
      env: {
        ...process.env,
        // These fixtures live outside a checkout.
        ALLOW_SOURCE_DRIFT: "1",
        // Ambient git config must not reach the fixture's own git queries.
        GIT_CONFIG_GLOBAL: "/dev/null",
        GIT_CONFIG_SYSTEM: "/dev/null",
        ...extraEnv,
        PATH: `${bin}:${process.env.PATH ?? ""}`,
      },
    });
  return Object.assign(run, { log, script, bin });
}

/** Both streams and the status, for a path that warns rather than exiting non-zero. */
function capture(run: Runner, ...args: string[]): { out: string; status: number | null } {
  const r = spawnSync("bash", [run.script, ...args], {
    encoding: "utf8",
    env: {
      ...process.env,
      ALLOW_SOURCE_DRIFT: "0",
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      PATH: `${run.bin}:${process.env.PATH ?? ""}`,
    },
  });
  return { out: `${r.stdout}${r.stderr}`, status: r.status };
}

const NODE_FIXTURE = [
  "worker1||",
  "worker2||",
  "master1||NoSchedule;",
  "notready1||NoExecute;",
  "prefer1||PreferNoSchedule;",
  "cordoned1|true|",
  "both1||PreferNoSchedule;NoExecute;",
  "",
].join("\n");
const ELIGIBLE = ["worker1", "worker2", "prefer1"];
const WARM_PODS = [...ELIGIBLE, ""].join("\n");
const DIGEST = "docker-pullable://x@sha256:aaaa";
const warmRows = (digests: string[]): string =>
  [
    ...ELIGIBLE.map(
      (n, i) =>
        `${n}|Succeeded|2026-08-18T00:00:00Z|2026-08-18T00:02:00Z|${digests[i] ?? digests[0]}`,
    ),
    "",
  ].join("\n");

describe("fleet image pin ↔ pull policy ↔ pre-pull (#2294)", () => {
  it("finds a bots-app container in every manifest with a bots-app image: line", () => {
    const lineRe = new RegExp(`^\\s*-?\\s*image:\\s*\\S*${BOT_IMAGE_REPO}`, "m");
    const named = manifestFiles().filter((f) => lineRe.test(readFileSync(resolve(K8S, f), "utf8")));
    expect(named.length).toBeGreaterThan(2);
    const walked = new Set(botImageRefs().map((r) => r.file));
    expect(named.filter((f) => !walked.has(f))).toEqual([]);
  });

  it("makes every manifest that runs the bots-app image name the same reference", () => {
    const refs = botImageRefs();
    expect(refs.length).toBeGreaterThan(2);
    expect(new Set(refs.map((r) => r.image))).toHaveLength(1);
  });

  it("pins tag AND digest in every one of them, so a re-pushed tag cannot move", () => {
    const refs = botImageRefs();
    expect(refs.length).toBeGreaterThan(2);
    for (const r of refs) {
      expect(r.image, `${r.file}:${r.container}`).toMatch(PINNED_REF);
    }
  });

  it("uses IfNotPresent in every one of them, so the pre-pulled layers are what starts", () => {
    const refs = botImageRefs();
    expect(refs.length).toBeGreaterThan(2);
    for (const r of refs) {
      expect(r.policy, `${r.file}:${r.container}`).toBe("IfNotPresent");
    }
  });

  // Kubelet re-pulls a cached image when the pod's pull credentials differ from
  // the ones that fetched it, which would defeat IfNotPresent silently.
  it("pulls every one of them with the same credential", () => {
    const secrets = new Set(botImageRefs().map((r) => r.pullSecrets.join(",")));
    expect(secrets).toHaveLength(1);
    expect([...secrets][0]).toBe("hclcr-io");
  });

  it("leaves a manifest running some other image out of the invariant", () => {
    const other = podSpecs(readYaml<unknown>("netem-preload-daemonset.yaml")).flatMap(
      (s) => s.containers ?? [],
    );
    expect(other.length).toBeGreaterThan(0);
    for (const c of other) {
      expect(c.image).not.toContain(BOT_IMAGE_REPO);
      expect(botImageRefs().some((r) => r.image === c.image)).toBe(false);
    }
  });

  it("keeps the pre-pull template free of any hardcoded image reference", () => {
    const raw = readFileSync(resolve(K8S, TEMPLATE), "utf8");
    const doc = readYaml<{ spec: { template: { spec: PodSpec } } }>(TEMPLATE);
    const c = (doc.spec.template.spec.containers ?? [])[0];
    expect(c.image).toBe(IMAGE_PLACEHOLDER);
    expect(c.imagePullPolicy).toBe(POLICY_PLACEHOLDER);
    expect(raw).not.toMatch(/image:\s*\S*hclcr\.io/);
  });

  it("keeps every placeholder out of the appliable manifests", () => {
    const raw = readFileSync(resolve(K8S, TEMPLATE), "utf8");
    const tokens = [...new Set(raw.match(/__[A-Z_]+__/g) ?? [])];
    expect(tokens.length).toBeGreaterThan(2);
    for (const f of manifestFiles()) {
      for (const t of tokens)
        expect(readFileSync(resolve(K8S, f), "utf8"), `${f} ${t}`).not.toContain(t);
    }
  });

  it("renders the pre-pull Job with the one agreed image and IfNotPresent", () => {
    const c = (renderPrepull(4).spec.template.spec.containers ?? [])[0];
    expect(c.image).toBe(agreedImage());
    expect(c.imagePullPolicy).toBe("IfNotPresent");
  });

  it("fans the pre-pull out one pod per node, and never onto a tainted master", () => {
    const rendered = renderPrepull(7);
    expect(rendered.spec.completions).toBe(7);
    expect(rendered.spec.parallelism).toBe(7);
    const spec = rendered.spec.template.spec;
    expect(spec.tolerations).toBeUndefined();
    const anti = spec.affinity?.podAntiAffinity?.requiredDuringSchedulingIgnoredDuringExecution;
    expect(anti?.map((r) => r.topologyKey)).toEqual(["kubernetes.io/hostname"]);
  });

  // The template is not a .yaml, so botImageRefs() cannot see it; these are its
  // only guard, and they run it through the production render path.
  it("pulls the pre-pull with the same credential the fleet pulls with", () => {
    const secrets = (renderPrepull(1).spec.template.spec.imagePullSecrets ?? []).map((s) => s.name);
    const fleet = [...new Set(botImageRefs().flatMap((r) => r.pullSecrets))];
    expect(fleet.length).toBeGreaterThan(0);
    expect(secrets).toEqual(fleet);
  });

  it("runs the pre-pull unprivileged, non-root, and without a retry", () => {
    const spec = renderPrepull(1).spec;
    expect(spec.backoffLimit).toBe(0);
    expect(spec.activeDeadlineSeconds).toBeGreaterThan(0);
    expect(spec.template.spec.restartPolicy).toBe("Never");
    expect(spec.template.spec.securityContext?.runAsNonRoot).toBe(true);
    const c = (spec.template.spec.containers ?? [])[0];
    expect(c.securityContext?.allowPrivilegeEscalation).toBe(false);
    expect(c.securityContext?.capabilities?.drop).toEqual(["ALL"]);
    expect(c.securityContext?.seccompProfile?.type).toBe("RuntimeDefault");
  });

  it("warms the reference every manifest agreed on, read from the manifest not a default", () => {
    const out = execFileSync("bash", [PREPULL_SH, "--print-image"], { encoding: "utf8" }).trim();
    expect(out).toBe(agreedImage());
  });

  it("passes its own agreement check against the real manifests", () => {
    const out = execFileSync("bash", [PREPULL_SH, "--check-agreement"], { encoding: "utf8" });
    expect(out).toContain(agreedImage());
  });

  it("reports the policy it will warm under", () => {
    const out = execFileSync("bash", [PREPULL_SH, "--print-policy"], { encoding: "utf8" }).trim();
    expect(out).toBe("IfNotPresent");
  });

  it("warms soft-tainted nodes but never hard-tainted or cordoned ones", () => {
    const admitted = withStubbedKubectl({ nodes: NODE_FIXTURE })("--print-nodes")
      .split("\n")
      .filter((l) => l !== "");
    expect(admitted).toEqual(ELIGIBLE);
  });

  it("refuses to warm when the scan finds no eligible node", () => {
    const run = withStubbedKubectl({
      nodes: ["master1||NoSchedule;", "notready1||NoExecute;", ""].join("\n"),
    });
    expect(() => run("run")).toThrow(/no schedulable, untainted nodes found/);
  });

  it("warms every eligible node and says so, end to end", () => {
    const out = withStubbedKubectl({
      nodes: NODE_FIXTURE,
      rows: warmRows([DIGEST]),
      pods: WARM_PODS,
    })("run");
    expect(out).toContain("on 3 node(s)");
    expect(out).toContain("prepull: complete");
    expect(out).toContain(`all warmed nodes agree on digest ${DIGEST}`);
    expect(out).toContain("every schedulable node is warm");
    // An empty section must say it is empty, not print a bare header.
    expect(out).toMatch(/pull durations:\n {2}\(none reported/);
    expect(out).toMatch(/clock samples[^\n]*\n {2}\(none reported/);
  });

  it("fails the run when a schedulable node ran no pre-pull pod", () => {
    const run = withStubbedKubectl({
      nodes: NODE_FIXTURE,
      rows: warmRows([DIGEST]),
      pods: ["worker1", "worker2", ""].join("\n"),
    });
    expect(() => run("run")).toThrow(/schedulable but NOT warm: prefer1/);
  });

  it("fails the run when no pre-pull pod is left to prove coverage", () => {
    const run = withStubbedKubectl({ nodes: NODE_FIXTURE, rows: warmRows([DIGEST]), pods: "" });
    expect(() => run("run")).toThrow(/schedulable but NOT warm: prefer1 worker1 worker2/);
  });

  it("counts only pods that finished, not ones still pulling", () => {
    const run = withStubbedKubectl({
      nodes: NODE_FIXTURE,
      rows: warmRows([DIGEST]),
      pods: WARM_PODS,
    });
    run("run");
    expect(readFileSync(run.log, "utf8")).toContain('phase=="Succeeded"');
  });

  // FAST_LOOP bounds the wait so a regression that fails to stop fails the test
  // in seconds instead of hanging it for the full deadline.
  const FAST_LOOP = { POLL_SECONDS: "1", DEADLINE_SECONDS: "1", BACKOFF_GRACE_POLLS: "2" };

  it("stops when it cannot read the Job at all, instead of waiting out the deadline", () => {
    const run = withStubbedKubectl({ nodes: NODE_FIXTURE, jobFails: true }, undefined, FAST_LOOP);
    expect(() => run("run")).toThrow(/cannot tell whether the warm-up finished/);
  });

  it("names an unusable image reference in the wait loop, not after 30 minutes", () => {
    const run = withStubbedKubectl(
      {
        nodes: NODE_FIXTURE,
        conds: "Complete=False\n",
        waiting: ["worker1 InvalidImageName", ""].join("\n"),
      },
      undefined,
      FAST_LOOP,
    );
    expect(() => run("run")).toThrow(/image reference is unusable: worker1 InvalidImageName/);
  });

  it("gives up on a pull that keeps failing, and says which node and why", () => {
    const run = withStubbedKubectl(
      {
        nodes: NODE_FIXTURE,
        conds: "Complete=False\n",
        waiting: ["worker2 ImagePullBackOff", ""].join("\n"),
      },
      undefined,
      FAST_LOOP,
    );
    expect(() => run("run")).toThrow(/still failing to pull after 2s: worker2 ImagePullBackOff/);
  });

  it("fails the run when the tag resolved to different digests mid-warm-up", () => {
    const run = withStubbedKubectl({
      nodes: NODE_FIXTURE,
      rows: warmRows([DIGEST, DIGEST, "docker-pullable://x@sha256:bbbb"]),
      pods: WARM_PODS,
    });
    expect(() => run("run")).toThrow(/resolved the tag to DIFFERENT digests/);
  });

  it("refuses to warm when one manifest names a different image", () => {
    const dir = manifestCopy();
    const drifted = resolve(dir, "conductor-job.yaml");
    writeFileSync(
      drifted,
      readFileSync(drifted, "utf8").replace(
        agreedImage(),
        `hclcr.io/hcllabs/${BOT_IMAGE_REPO}:latest`,
      ),
    );
    expect(readFileSync(drifted, "utf8")).toContain(":latest");
    const run = withStubbedKubectl({ nodes: NODE_FIXTURE, pods: WARM_PODS }, dir);
    expect(() => run("--check-agreement")).toThrow(/disagree on the bots-app image/);
    // The flag is a diagnostic; `run` must refuse on its own.
    expect(() => run("run")).toThrow(/disagree on the bots-app image/);
  });

  it("refuses to warm under a policy that re-pulls anyway", () => {
    const dir = manifestCopy();
    const sts = resolve(dir, "statefulset.yaml");
    writeFileSync(
      sts,
      readFileSync(sts, "utf8").replace("imagePullPolicy: IfNotPresent", "imagePullPolicy: Always"),
    );
    const run = withStubbedKubectl({ nodes: NODE_FIXTURE, pods: WARM_PODS }, dir);
    expect(() => run("run")).toThrow(/imagePullPolicy is Always/);
  });

  // A background cascade leaves the previous generation's pods behind, and
  // report() selects by name label, so their stale imageID reads as tag drift.
  it("waits out the previous Job's pods before warming again", () => {
    const run = withStubbedKubectl({
      nodes: NODE_FIXTURE,
      rows: warmRows([DIGEST]),
      pods: WARM_PODS,
    });
    run("run");
    expect(readFileSync(run.log, "utf8")).toMatch(/delete job \S+ .*--cascade=foreground --wait/);
  });

  it("counts a .yml manifest in the agreement, not just .yaml", () => {
    const dir = manifestCopy();
    writeFileSync(
      resolve(dir, "zz-probe.yml"),
      `image: hclcr.io/hcllabs/${BOT_IMAGE_REPO}:latest\n`,
    );
    expect(() => withStubbedKubectl({}, dir)("--check-agreement")).toThrow(
      /disagree on the bots-app image/,
    );
  });

  it("counts a manifest whose filename contains a space", () => {
    const dir = manifestCopy();
    writeFileSync(
      resolve(dir, "zz probe.yml"),
      `image: hclcr.io/hcllabs/${BOT_IMAGE_REPO}:latest\n`,
    );
    expect(() => withStubbedKubectl({}, dir)("--check-agreement")).toThrow(
      /disagree on the bots-app image/,
    );
  });

  // grep exits 2 on an unreadable file and 1 on no-match; conflating them
  // reports agreement over manifests that were never scanned.
  it.skipIf(process.getuid?.() === 0)("refuses to report agreement it could not verify", () => {
    const dir = manifestCopy();
    chmodSync(resolve(dir, "conductor-job.yaml"), 0o000);
    expect(() => withStubbedKubectl({}, dir)("--check-agreement")).toThrow(
      /could not read every manifest/,
    );
  });

  it("refuses an image reference carrying shell or sed metacharacters", () => {
    const dir = manifestCopy();
    const sts = resolve(dir, "statefulset.yaml");
    writeFileSync(sts, readFileSync(sts, "utf8").replace(agreedImage(), "x$(touch /dev/null)|e#"));
    expect(() => withStubbedKubectl({}, dir)("--render", "1")).toThrow(
      /illegal character in image/,
    );
  });

  it("keeps the operator docs naming every manifest that must move together", () => {
    const readme = readFileSync(resolve(K8S, "..", "README.md"), "utf8");
    expect(readme).toContain("prepull-image.sh");
    const build = readFileSync(resolve(K8S, "..", "build.sh"), "utf8");
    const pinned = [...new Set(botImageRefs().map((r) => r.file.replace(/\.ya?ml$/, "")))];
    expect(pinned.length).toBeGreaterThan(2);
    for (const f of pinned) expect(build, `build.sh must name ${f}`).toContain(f);
  });
});

const REPO_ROOT = resolve(K8S, "..", "..", "..");
const WORKFLOW = resolve(REPO_ROOT, ".github", "workflows", "build-bots-image-hcl.yaml");
const PR_CHECK = resolve(REPO_ROOT, ".github", "workflows", "pr-check-e2e-lint-hcl.yaml");
const BUILD_SH = resolve(K8S, "..", "build.sh");
const DOCKERFILE = resolve(K8S, "..", "Dockerfile");
const FIXTURE_SRC = "e2e/bots-app/src/fixture-source.ts";
/** Not in the image, but it decides what of e2e/ reaches it. */
const CONTEXT_FILTER = ".dockerignore";
const IN_IMAGE_BUT_INERT = [
  "e2e/tests/fixture.spec.ts",
  "e2e/bots-app/fixture.test.ts",
  "e2e/bots-app/dashboard/src/__tests__/Fixture.test.tsx",
  "e2e/bots-app/dashboard/src/Fixture.tsx",
  "e2e/README.md",
];
/** Outside the build context, so no rebuild can be owed for it. */
const OUTSIDE_IMAGE = "dioxus-ui/src/fixture.rs";
/** git-ignored, so the untracked-file scan must not mistake it for drift. */
const IGNORED_ARTIFACT = "e2e/node_modules/fixture/index.js";
/**
 * Real files a bot runs from inside the image. They anchor the scan on the repo
 * rather than on the workflow's copy of the same list.
 */
const MUST_BE_SCANNED = [
  "e2e/bots-app/docker-entrypoint.sh",
  "e2e/bots-app/Dockerfile",
  "e2e/bots-app/src/cli.ts",
  "e2e/bots-app/src/bot.ts",
  "e2e/helpers/auth.ts",
  "e2e/package.json",
  "e2e/package-lock.json",
];

/** git treats `e2e` and `e2e/` alike, so every consumer of DRIFT_PATHS must too. */
const trimSlash = (p: string) => p.replace(/\/+$/, "");

function git(cwd: string, ...args: string[]): string {
  return execFileSync("git", ["-C", cwd, ...args], {
    encoding: "utf8",
    stdio: "pipe",
    env: {
      ...process.env,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      GIT_AUTHOR_NAME: "t",
      GIT_AUTHOR_EMAIL: "t@t",
      GIT_COMMITTER_NAME: "t",
      GIT_COMMITTER_EMAIL: "t@t",
    },
  });
}

const pinFor = (commit: string): string =>
  `hclcr.io/hcllabs/${BOT_IMAGE_REPO}:0.1.0-20260814-${commit}@sha256:${"a".repeat(64)}`;

/** The production re-pin, so a fixture's pin is the one CI would write. */
function repinScript(k8sDir: string, commit: string): string {
  const ref = pinFor(commit);
  execFileSync("bash", [resolve(k8sDir, "repin.sh"), ref], { encoding: "utf8", stdio: "pipe" });
  return ref;
}

/** For refs repin.sh refuses on sight, which the gate must still reject. */
function repinByHand(k8sDir: string, commit: string): void {
  const want = pinFor(commit);
  let moved = 0;
  for (const f of manifestFiles()) {
    const p = resolve(k8sDir, f);
    const before = readFileSync(p, "utf8");
    const after = before.replace(new RegExp(`\\S*${BOT_IMAGE_REPO}:\\S+`, "g"), want);
    if (after === before) continue;
    writeFileSync(p, after);
    moved += 1;
  }
  expect(moved, "repin must move the pin, or the fixture proves nothing").toBeGreaterThan(2);
}

/**
 * A throwaway checkout laid out like the repo, so the gate's git queries run for
 * real. The pin can never be committed in the commit it names, so every fixture
 * leaves k8s/ dirty — which is why that directory is excluded from the scan.
 */
function gitFixture(): { root: string; k8s: string; commit: string; run: Runner } {
  const root = mkdtempSync(resolve(tmpdir(), "prepull-repo-"));
  const k8s = resolve(root, "e2e", "bots-app", "k8s");
  mkdirSync(k8s, { recursive: true });
  for (const rel of [
    FIXTURE_SRC,
    OUTSIDE_IMAGE,
    IGNORED_ARTIFACT,
    ...IN_IMAGE_BUT_INERT,
    ...MUST_BE_SCANNED,
  ]) {
    mkdirSync(resolve(root, rel, ".."), { recursive: true });
    writeFileSync(resolve(root, rel), "export const seeded = 1;\n");
  }
  writeFileSync(resolve(root, CONTEXT_FILTER), "**/node_modules\n");
  writeFileSync(resolve(root, ".gitignore"), "node_modules/\n");
  for (const f of [...manifestFiles(), TEMPLATE, "prepull-image.sh", "repin.sh"]) {
    copyFileSync(resolve(K8S, f), resolve(k8s, f));
  }
  git(root, "init", "-b", "main");
  git(root, "add", "-A");
  git(root, "commit", "-m", "fixture");
  const commit = git(root, "rev-parse", "--short=7", "HEAD").trim();
  repinScript(k8s, commit);
  return {
    root,
    k8s,
    commit,
    run: withStubbedKubectl({ nodes: NODE_FIXTURE, pods: WARM_PODS }, k8s, {
      ALLOW_SOURCE_DRIFT: "0",
    }),
  };
}

/** The workflow's `paths:` verbatim, minus the entries that only re-trigger CI. */
function workflowWatchedPaths(): string[] {
  const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as Record<string, unknown>;
  const on = (wf.on ?? wf[String(true)]) as { push?: { paths?: string[] } };
  const paths = on?.push?.paths ?? [];
  expect(paths, "the workflow must filter on paths").not.toEqual([]);
  return paths.filter((p) => !p.startsWith(".github/"));
}

/**
 * Each git pathspec as the GitHub glob selecting the same files. A GitHub `*`
 * stops at `/`, so `e2e` and `e2e/**` are different filters and the recursion
 * has to survive the translation.
 */
function expectedWorkflowPaths(): string[] {
  return driftPathsFromScript().map((spec) => {
    const excluded = spec.startsWith(":(exclude)");
    const path = trimSlash(excluded ? spec.slice(":(exclude)".length) : spec);
    let glob = path;
    if (path.startsWith("*")) {
      glob = `**/${path}`;
    } else {
      const full = resolve(REPO_ROOT, path);
      expect(existsSync(full), `DRIFT_PATHS names ${path}, which does not exist`).toBe(true);
      if (statSync(full).isDirectory()) glob = `${path}/**`;
    }
    return excluded ? `!${glob}` : glob;
  });
}

/** The repository the manifests pull, which is what build.sh must publish to. */
function pinnedRepo(): string {
  return agreedImage().split(":")[0];
}

/** Host paths the Dockerfile copies in: the source that ends up in the image. */
function dockerfileCopySources(): string[] {
  const srcs = new Set<string>();
  for (const line of readFileSync(DOCKERFILE, "utf8").split("\n")) {
    const m = /^\s*COPY\s+(.*)$/i.exec(line);
    if (!m || /--from=/.test(m[1])) continue;
    const words = m[1].split(/\s+/).filter((w) => !w.startsWith("--"));
    for (const w of words.slice(0, -1)) srcs.add(w);
  }
  return [...srcs].sort();
}

interface RunDefaults {
  run?: { shell?: string };
}
interface WorkflowStep {
  id?: string;
  if?: unknown;
  run?: string;
  env?: Record<string, string>;
  "continue-on-error"?: unknown;
}
interface BuildJob {
  steps?: WorkflowStep[];
  env?: Record<string, string>;
  defaults?: RunDefaults;
  "continue-on-error"?: unknown;
}
function buildWorkflow(): { defaults?: RunDefaults; job: BuildJob } {
  const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
    defaults?: RunDefaults;
    jobs?: Record<string, BuildJob>;
  };
  const job = wf.jobs?.build;
  expect(job, "the workflow has no build job").toBeDefined();
  return { defaults: wf.defaults, job: job! };
}

/**
 * Announces that the stub is inside the window under test, then waits for the
 * driver's release file. Bounded so a stub the driver never reaches cannot hang.
 */
function holdLines(hold: string): string[] {
  return [
    `if [ -n "\${${hold}:-}" ]; then`,
    `  : >"\${${hold}}"`,
    "  n=0",
    `  while [ ! -e "\${${hold}}.go" ] && [ "$n" -lt 200 ]; do n=$((n + 1)); sleep 0.05; done`,
    "fi",
  ];
}

function writeStubs(bin: string, log: string): void {
  writeFileSync(
    resolve(bin, "podman"),
    [
      "#!/bin/sh",
      `printf '%s\\n' "$*" >>'${log}'`,
      'if [ "$1" = login ]; then cat >/dev/null; fi',
      'if [ "$1" = push ]; then',
      ...holdLines("STUB_PUSH_HOLD"),
      "fi",
      "exit 0",
    ].join("\n"),
    { mode: 0o755 },
  );
  // Stubbed too, or the digest read reaches the real registry over the network.
  writeFileSync(
    resolve(bin, "skopeo"),
    [
      "#!/bin/sh",
      `printf 'skopeo %s\\n' "$*" >>'${log}'`,
      'if [ "$1" = login ]; then',
      "  cat >/dev/null",
      "  while [ $# -gt 0 ]; do",
      '    if [ "$1" = --authfile ]; then',
      // Real skopeo fatals on a zero-length authfile before any network I/O.
      '      if [ -e "$2" ] && [ ! -s "$2" ]; then exit 1; fi',
      '      printf \'{"auths":{}}\\n\' >"$2"',
      "    fi",
      "    shift",
      "  done",
      ...holdLines("STUB_LOGIN_HOLD").map((l) => `  ${l}`),
      "  exit 0",
      "fi",
      'if [ "$1" = inspect ]; then',
      '  if [ -n "${STUB_NO_DIGEST:-}" ]; then exit 1; fi',
      '  if [ -n "${STUB_BAD_DIGEST:-}" ]; then printf \'%s\\n\' "${STUB_BAD_DIGEST}"; exit 0; fi',
      `  printf 'sha256:%s\\n' '${"b".repeat(64)}'`,
      "fi",
      "exit 0",
    ].join("\n"),
    { mode: 0o755 },
  );
}

function buildEnv(
  bin: string,
  pin: string,
  extraEnv: Record<string, string>,
): Record<string, string | undefined> {
  return {
    ...process.env,
    REGISTRY: "hclcr.io",
    REGISTRY_USER: "",
    REGISTRY_PASS: "",
    PODMAN_REMOTE: "0",
    PUSH: "1",
    PINNED_REF_FILE: pin,
    ...extraEnv,
    PATH: `${bin}:${process.env.PATH ?? ""}`,
  };
}

/**
 * Runs build.sh against a `podman` stub that records its argv, so which tags are
 * built and pushed is read off the real script rather than grepped out of it.
 * REGISTRY and the credentials are pinned so an operator's exported ones cannot
 * decide which branch runs.
 */
function runBuild(extraEnv: Record<string, string>): {
  status: number | null;
  calls: string[];
  pinnedRef: string | null;
  out: string;
} {
  const bin = mkdtempSync(resolve(tmpdir(), "build-bin-"));
  const log = resolve(bin, "calls.log");
  const pin = resolve(bin, "pinned-ref");
  writeStubs(bin, log);
  try {
    const r = spawnSync("bash", [BUILD_SH], {
      encoding: "utf8",
      env: buildEnv(bin, pin, extraEnv),
    });
    return {
      status: r.status,
      calls: existsSync(log) ? readFileSync(log, "utf8").split("\n").filter(Boolean) : [],
      pinnedRef: existsSync(pin) ? readFileSync(pin, "utf8").trim() : null,
      out: `${r.stdout ?? ""}${r.stderr ?? ""}`,
    };
  } finally {
    rmSync(bin, { recursive: true, force: true });
  }
}

/**
 * SIGINTs the real script's own pid — not its process group — while a stub is
 * held inside `hold`. A bash-backgrounded script has SIGINT ignored at entry, so
 * signalling from a shell driver would reach no trap at all.
 */
async function runBuildCancelled(hold: "login" | "push"): Promise<{
  status: number | null;
  out: string;
  calls: string[];
  authdir: string | null;
  pinnedRef: string | null;
}> {
  const bin = mkdtempSync(resolve(tmpdir(), "build-cancel-"));
  const log = resolve(bin, "calls.log");
  const pin = resolve(bin, "pinned-ref");
  const ready = resolve(bin, "ready");
  writeStubs(bin, log);
  try {
    const child = spawn("bash", [BUILD_SH], {
      detached: true,
      env: {
        ...buildEnv(bin, pin, { REGISTRY_USER: "u", REGISTRY_PASS: "p", PUSH_LATEST: "0" }),
        [hold === "login" ? "STUB_LOGIN_HOLD" : "STUB_PUSH_HOLD"]: ready,
      },
    });
    let out = "";
    child.stdout.on("data", (d: Buffer) => (out += d.toString()));
    child.stderr.on("data", (d: Buffer) => (out += d.toString()));
    const status = await new Promise<number | null>((res) => {
      const deadline = Date.now() + 4000;
      let signalled = false;
      const poll = setInterval(() => {
        if (!signalled && existsSync(ready)) {
          signalled = true;
          // Signal first, release second: the hold window has to still be open
          // when the signal arrives. The deadline stays armed either way.
          child.kill("SIGINT");
          writeFileSync(`${ready}.go`, "");
        } else if (Date.now() > deadline) {
          clearInterval(poll);
          // Group-wide — `detached` above makes the script the leader — because a
          // surviving stub child holds open the pipes `close` waits on.
          try {
            process.kill(-child.pid!, "SIGKILL");
          } catch {
            child.kill("SIGKILL");
          }
        }
      }, 20);
      child.on("close", (code) => {
        clearInterval(poll);
        res(code);
      });
    });
    const calls = (existsSync(log) ? readFileSync(log, "utf8") : "").split("\n").filter(Boolean);
    const authfile = /--authfile (\S+)/.exec(calls.find((c) => c.startsWith("skopeo login")) ?? "");
    return {
      status,
      out,
      calls,
      authdir: authfile ? resolve(authfile[1], "..") : null,
      pinnedRef: existsSync(pin) ? readFileSync(pin, "utf8").trim() : null,
    };
  } finally {
    rmSync(bin, { recursive: true, force: true });
  }
}

function driftPathsFromScript(): string[] {
  const found = readFileSync(PREPULL_SH, "utf8").match(/DRIFT_PATHS=\(\n([\s\S]*?)\n\)/);
  expect(found, "DRIFT_PATHS not found in prepull-image.sh").not.toBeNull();
  return found![1]
    .split("\n")
    .map((l) => l.trim().replace(/^'(.*)'$/, "$1"))
    .filter((l) => l.length > 0 && !l.startsWith("#"))
    .sort();
}

describe("pinned image ↔ the source it ships (#2293)", () => {
  it("reads the commit out of the pinned tag, not the digest that follows it", () => {
    const image = agreedImage();
    const expected = image.split("@")[0].split(":").pop()!.split("-").pop();
    const printed = withStubbedKubectl({})("--print-pinned-commit").trim();
    expect(printed).toBe(expected);
    expect(printed).not.toBe(image.split("sha256:")[1]);
  });

  it("warms when the only thing differing from the pinned commit is the pin", () => {
    const { commit, run } = gitFixture();
    expect(run("--check-source-drift")).toContain(`matches the tree at ${commit}`);
  });

  it("refuses to warm when a file that ships inside the image differs, and names it", () => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, FIXTURE_SRC), "export const seeded = 2;\n");
    expect(() => run("--check-source-drift")).toThrow(
      new RegExp(`ship INSIDE it[\\s\\S]*${FIXTURE_SRC}`),
    );
  });

  it("refuses to warm over a file added since the pin, which no diff would report", () => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, "e2e", "bots-app", "src", "added.ts"), "export const late = 1;\n");
    expect(() => run("--check-source-drift")).toThrow(/e2e\/bots-app\/src\/added\.ts/);
  });

  it.each(IN_IMAGE_BUT_INERT)("keeps warming when only %s differs", (rel) => {
    const { root, commit, run } = gitFixture();
    writeFileSync(resolve(root, rel), "export const seeded = 2;\n");
    expect(run("--check-source-drift")).toContain(`matches the tree at ${commit}`);
  });

  it("keeps warming when the change is outside the image's build context", () => {
    const { root, commit, run } = gitFixture();
    writeFileSync(resolve(root, OUTSIDE_IMAGE), "fn main() {}\n");
    expect(run("--check-source-drift")).toContain(`matches the tree at ${commit}`);
  });

  it("keeps warming over a git-ignored artifact, which is in no image", () => {
    const { root, commit, run } = gitFixture();
    writeFileSync(resolve(root, IGNORED_ARTIFACT), "late\n");
    writeFileSync(resolve(root, "e2e", "node_modules", "fixture", "extra.js"), "late\n");
    expect(run("--check-source-drift")).toContain(`matches the tree at ${commit}`);
  });

  it("refuses a pin whose commit is too short to name one build", () => {
    const { k8s, run } = gitFixture();
    repinByHand(k8s, "abc");
    expect(() => run("--check-source-drift")).toThrow(/too ambiguous to resolve/);
  });

  it("refuses to warm when the pinned commit is absent from this clone", () => {
    const { k8s, run } = gitFixture();
    repinScript(k8s, "deadbee");
    expect(() => run("--check-source-drift")).toThrow(/deadbee, which this clone does not have/);
  });

  it("refuses a pin carrying no commit, so a moving tag can never satisfy it", () => {
    const { k8s, run } = gitFixture();
    for (const f of manifestFiles()) {
      const p = resolve(k8s, f);
      writeFileSync(
        p,
        readFileSync(p, "utf8").replace(/:0\.1\.0-\d{8}-[0-9a-f]{7,}@/g, ":latest@"),
      );
    }
    expect(() => run("--check-source-drift")).toThrow(/cannot read a commit from the pinned tag/);
  });

  it("refuses to warm outside a checkout rather than assuming the pin is current", () => {
    const dir = manifestCopy();
    expect(() =>
      withStubbedKubectl({}, dir, { ALLOW_SOURCE_DRIFT: "0" })("--check-source-drift"),
    ).toThrow(/is not a git checkout/);
  });

  it("warms a stale pin only when the operator asks for it, and says so", () => {
    const { root, k8s } = gitFixture();
    writeFileSync(resolve(root, FIXTURE_SRC), "export const seeded = 2;\n");
    const out = withStubbedKubectl({ nodes: NODE_FIXTURE, pods: WARM_PODS }, k8s, {
      ALLOW_SOURCE_DRIFT: "1",
    })("--check-source-drift");
    expect(out).toContain("NOT checking the pinned image against the tree");
  });

  it("still answers which nodes are warm when the tree moved, but says the pin is stale", () => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, FIXTURE_SRC), "export const seeded = 2;\n");
    const r = capture(run, "--verify-coverage");
    expect(r.status, "a cluster read must not fail on working-tree state").toBe(0);
    expect(r.out).toMatch(/WARNING[\s\S]*ship INSIDE it/);
    expect(r.out).toContain("every schedulable node is warm");
  });

  it("names git as what is missing, instead of blaming a checkout that is fine", () => {
    const { k8s } = gitFixture();
    // An empty PATH is the point, so bash cannot be resolved through it either.
    const bash = execFileSync("bash", ["-c", "command -v bash"], { encoding: "utf8" }).trim();
    const empty = mkdtempSync(resolve(tmpdir(), "nogit-"));
    const r = spawnSync(bash, [resolve(k8s, "prepull-image.sh"), "--check-source-drift"], {
      encoding: "utf8",
      env: { PATH: empty, ALLOW_SOURCE_DRIFT: "0" },
    });
    expect(r.status).not.toBe(0);
    expect(`${r.stdout}${r.stderr}`).toMatch(/git is not on PATH/);
  });

  it("blocks the warm-up itself before it touches the cluster", () => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, FIXTURE_SRC), "export const seeded = 2;\n");
    expect(() => run("run")).toThrow(/ship INSIDE it/);
    expect(existsSync(run.log), "must not reach kubectl at all").toBe(false);
  });

  it("builds a new image for exactly the paths the gate scans", () => {
    expect(workflowWatchedPaths().sort()).toEqual(expectedWorkflowPaths().sort());
  });

  it("puts the broad include ahead of every exclude, because a later match wins", () => {
    const paths = workflowWatchedPaths();
    expect(paths[0]).toBe("e2e/**");
    const firstExclude = paths.findIndex((p) => p.startsWith("!"));
    expect(firstExclude).toBeGreaterThan(0);
    const reincluded = paths
      .slice(firstExclude)
      .filter((p) => !p.startsWith("!") && p.includes("*"));
    expect(reincluded, "a wildcard include after an exclude undoes it").toEqual([]);
  });

  it("scans every host path the Dockerfile copies into the image", () => {
    const specs = driftPathsFromScript();
    const positives = specs.filter((p) => !p.startsWith(":(exclude)")).map(trimSlash);
    const excludes = specs
      .filter((p) => p.startsWith(":(exclude)"))
      .map((p) => trimSlash(p.slice(":(exclude)".length)));
    const sources = dockerfileCopySources();
    expect(sources.length).toBeGreaterThan(0);
    for (const raw of sources) {
      const src = trimSlash(raw);
      const under = (p: string) => src === p || src.startsWith(`${p}/`);
      expect(positives.some(under), `${src} reaches the image but no drift path covers it`).toBe(
        true,
      );
      // An exclude may trim inside a copied tree, never swallow one whole.
      expect(excludes.some(under), `${src} reaches the image but an exclude removes it`).toBe(
        false,
      );
    }
  });

  it("anchors the scan on real files, so no fixture pins a path the repo dropped", () => {
    for (const rel of MUST_BE_SCANNED) {
      expect(existsSync(resolve(REPO_ROOT, rel)), `${rel} is gone — the anchor is stale`).toBe(
        true,
      );
    }
  });

  it.each(MUST_BE_SCANNED)("refuses to warm when %s differs, since the image runs it", (rel) => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, rel), "export const changed = 2;\n");
    expect(() => run("--check-source-drift")).toThrow(
      new RegExp(`ship INSIDE it[\\s\\S]*${rel.replace(/\./g, "\\.")}`),
    );
  });

  it("excludes the dashboard only while nothing builds it into the image", () => {
    expect(readFileSync(resolve(REPO_ROOT, CONTEXT_FILTER), "utf8")).toMatch(/^\*\*\/dist$/m);
    expect(readFileSync(DOCKERFILE, "utf8")).not.toMatch(/dashboard/i);
    expect(driftPathsFromScript()).toContain(":(exclude)e2e/bots-app/dashboard");
  });

  it("rebuilds on a change to the build workflow itself, or it can never be exercised", () => {
    const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { push?: { paths?: string[] } };
    expect(on?.push?.paths).toContain(".github/workflows/build-bots-image-hcl.yaml");
  });

  it("runs this suite on every artifact it asserts against, or the lock goes unread", () => {
    const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    expect(paths).toContain(".github/workflows/build-bots-image-hcl.yaml");
    expect(paths).toContain(CONTEXT_FILTER);
  });

  it("serializes on the one condition that moves :latest, and cancels nothing", () => {
    const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
      concurrency?: { group?: string; "cancel-in-progress"?: boolean };
      jobs?: { build?: { steps?: Array<{ id?: string; env?: Record<string, string> }> } };
    };
    const step = wf.jobs?.build?.steps?.find((s) => s.id === "build");
    const cond = "github.ref_name == 'hcl-main' || inputs.push_latest";
    expect(step?.env?.PUSH_LATEST, "the :latest gate moved").toContain(cond);
    expect(wf.concurrency?.group, "the group must bucket on the same gate").toContain(cond);
    expect(wf.concurrency?.["cancel-in-progress"]).toBe(false);
  });

  it("refuses when .dockerignore moved, which silently changes what the image holds", () => {
    const { root, run } = gitFixture();
    writeFileSync(resolve(root, CONTEXT_FILTER), "**/node_modules\ne2e/bots-app/src\n");
    expect(() => run("--check-source-drift")).toThrow(/\.dockerignore/);
  });

  it("moves :latest only on the release branch", () => {
    const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
      jobs?: { build?: { steps?: Array<{ id?: string; env?: Record<string, string> }> } };
    };
    const step = wf.jobs?.build?.steps?.find((s) => s.id === "build");
    expect(step?.env?.PUSH_LATEST).toBe(
      "${{ (github.ref_name == 'hcl-main' || inputs.push_latest) && '1' || '0' }}",
    );
  });
});

describe("build.sh tag and digest handling (#2293)", () => {
  it("leaves :latest untagged and unpushed when told not to move it", () => {
    const { calls } = runBuild({ PUSH_LATEST: "0" });
    const build = calls.find((c) => c.startsWith("build "));
    expect(build, "build.sh never invoked the builder").toBeDefined();
    expect(build).not.toContain(":latest");
    const pushes = calls.filter((c) => c.startsWith("push "));
    expect(pushes).toHaveLength(1);
    expect(pushes[0]).not.toContain(":latest");
  });

  it("publishes the alias alongside the dated tag when asked", () => {
    const { calls } = runBuild({ PUSH_LATEST: "1" });
    expect(calls.find((c) => c.startsWith("build "))).toContain(":latest");
    const pushes = calls.filter((c) => c.startsWith("push "));
    expect(pushes).toHaveLength(2);
    expect(pushes.filter((p) => p.endsWith(":latest"))).toHaveLength(1);
  });

  it("reports a reference to pin only when a digest resolved", () => {
    const ok = runBuild({ PUSH_LATEST: "0" });
    expect(ok.pinnedRef).toMatch(new RegExp(`^${pinnedRepo()}:\\S+@sha256:[0-9a-f]{64}$`));
    expect(ok.status, "a resolved digest still failed the script").toBe(0);
    const none = runBuild({ PUSH_LATEST: "0", STUB_NO_DIGEST: "1" });
    expect(none.pinnedRef).toBeNull();
    expect(none.status, "an unread digest exited 0 for a caller that asked for a pin").not.toBe(0);
  });

  it("exits 0 on an unread digest when no caller asked for a pin", () => {
    const r = runBuild({ PUSH_LATEST: "0", STUB_NO_DIGEST: "1", PINNED_REF_FILE: "" });
    expect(r.status, "a build-only run was failed by the digest read").toBe(0);
    expect(r.out).toContain("Could not read the registry digest");
  });

  it("reads the digest off the registry for the pushed tag, not the local store", () => {
    const { calls, pinnedRef } = runBuild({ PUSH_LATEST: "0" });
    const inspect = calls.find((c) => c.startsWith("skopeo inspect"));
    expect(inspect, "build.sh never asked the registry for a digest").toBeDefined();
    expect(inspect).toContain(`docker://${pinnedRef!.split("@")[0]}`);
    expect(calls.filter((c) => /^inspect\b/.test(c))).toHaveLength(0);
    const push = calls.findIndex((c) => c.startsWith("push "));
    expect(push, "nothing was pushed").toBeGreaterThanOrEqual(0);
    expect(push, "the digest was read before the push that produces it").toBeLessThan(
      calls.findIndex((c) => c.startsWith("skopeo inspect")),
    );
  });

  it("authenticates the digest read, which the builder-side push login does not cover", () => {
    const { calls } = runBuild({ PUSH_LATEST: "0", REGISTRY_USER: "u", REGISTRY_PASS: "p" });
    const authfile = /--authfile (\S+)/.exec(calls.find((c) => c.startsWith("skopeo login"))!);
    expect(authfile, "no skopeo login wrote an authfile").not.toBeNull();
    expect(calls.find((c) => c.startsWith("skopeo inspect"))).toContain(
      `--authfile ${authfile![1]}`,
    );
    expect(existsSync(authfile![1]), "the credential outlived the script").toBe(false);
  });

  it("refuses a digest that is not a well-formed sha256", () => {
    const hex = "a".repeat(64);
    for (const bad of [
      "not-a-digest",
      "sha256:abc",
      `sha256:${"z".repeat(64)}`,
      `sha256:${hex.slice(0, 63)}z`,
      `sha256:${hex.toUpperCase()}`,
      `sha256:${hex}a`,
      `sha512:${hex}`,
      `xsha256:${hex}`,
    ]) {
      const r = runBuild({ PUSH_LATEST: "0", STUB_BAD_DIGEST: bad });
      expect(r.pinnedRef, `${bad} was pinned`).toBeNull();
      expect(r.status, `${bad} exited 0`).not.toBe(0);
    }
  });

  it("moves :latest when unasked, and refuses a value it does not understand", () => {
    const { calls } = runBuild({});
    expect(calls.find((c) => c.startsWith("build "))).toContain(":latest");
    expect(calls.filter((c) => c.startsWith("push "))).toHaveLength(2);
    const bad = runBuild({ PUSH_LATEST: "true" });
    expect(bad.status, "an unparseable PUSH_LATEST was accepted").not.toBe(0);
    expect(bad.out).toMatch(/must be 0 or 1/);
  });

  it("never advises pushing a :latest the build did not create", () => {
    expect(runBuild({ PUSH: "0", PUSH_LATEST: "0" }).out).not.toContain(`${pinnedRepo()}:latest`);
    expect(runBuild({ PUSH: "0", PUSH_LATEST: "1" }).out).toContain(`${pinnedRepo()}:latest`);
  });

  it("is preflighted by a step that really exits nonzero when skopeo is absent", () => {
    const wf = buildWorkflow();
    const steps = wf.job.steps ?? [];
    const pre = steps.findIndex((s) => s.id === "preflight");
    expect(pre, "no step with id preflight").toBeGreaterThanOrEqual(0);
    expect(pre, "a preflight ordered after the push cannot gate it").toBeLessThan(
      steps.findIndex((s) => s.id === "build"),
    );
    expect(steps[pre].if, "a conditional preflight does not fail closed").toBeUndefined();
    expect(
      steps[pre]["continue-on-error"],
      "a continue-on-error preflight does not fail closed",
    ).toBeUndefined();
    expect(
      wf.job["continue-on-error"],
      "a continue-on-error job discards the preflight's failure",
    ).toBeUndefined();

    const dir = mkdtempSync(resolve(tmpdir(), "preflight-"));
    try {
      const body = resolve(dir, "step");
      writeFileSync(body, steps[pre].run ?? "");
      const shell = (wf.job.defaults?.run?.shell ?? wf.defaults?.run?.shell ?? "bash {0}").split(
        /\s+/,
      );
      // PATH is replaced wholesale to hide skopeo, so the shell needs its real path.
      const argv = shell.map((w) => (w === "{0}" ? body : w));
      argv[0] = spawnSync("sh", ["-c", `command -v ${argv[0]}`], {
        encoding: "utf8",
      }).stdout.trim();
      expect(argv[0], `${shell[0]} is not on PATH`).not.toBe("");
      const status = () =>
        spawnSync(argv[0], argv.slice(1), { encoding: "utf8", env: { PATH: dir } }).status;
      expect(status(), "the preflight did not fail with no skopeo on PATH").toBeGreaterThan(0);
      writeFileSync(resolve(dir, "skopeo"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
      expect(status(), "the preflight failed with skopeo present").toBe(0);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  const CURL_STUB = (body: string) =>
    [
      "#!/bin/sh",
      'out=""; prev=""',
      'for a in "$@"; do',
      '  if [ "$prev" = "-o" ]; then out="$a"; fi',
      '  url="$a"; prev="$a"',
      "done",
      "printf '%s\\n' \"$url\" >> __LOG__",
      body,
    ].join("\n");

  /** Runs the real install step against a stub curl, so its own paths execute. */
  function runInstall(stub: string): {
    status: number | null;
    out: string;
    ghPath: string;
    urls: string[];
  } {
    const step = (buildWorkflow().job.steps ?? []).find((s) => s.id === "install-skopeo");
    expect(step, "no step with id install-skopeo").toBeDefined();
    const dir = mkdtempSync(resolve(tmpdir(), "install-skopeo-"));
    try {
      const bin = resolve(dir, "bin");
      mkdirSync(bin);
      const log = resolve(dir, "curl.log");
      writeFileSync(resolve(bin, "curl"), CURL_STUB(stub).replace(/__LOG__/g, log), {
        mode: 0o755,
      });
      // The step body resolves every tool through this bin and nothing else, so
      // a skopeo on the host PATH cannot early-exit it and void the assertions.
      for (const tool of ["bash", "rm", "mkdir", "sha256sum"]) {
        const real = spawnSync("sh", ["-c", `command -v ${tool}`], {
          encoding: "utf8",
        }).stdout.trim();
        expect(real, `${tool} is not on PATH`).not.toBe("");
        symlinkSync(real, resolve(bin, tool));
      }
      const ghPath = resolve(dir, "gh-path");
      writeFileSync(ghPath, "");
      const r = runStep("build", (s) => s.id === "install-skopeo", {
        ...(step!.env ?? {}),
        SKOPEODIR: resolve(dir, "unpacked"),
        GITHUB_PATH: ghPath,
        PATH: bin,
      });
      return {
        ...r,
        ghPath: readFileSync(ghPath, "utf8"),
        urls: existsSync(log) ? readFileSync(log, "utf8").split("\n").filter(Boolean) : [],
      };
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  it("installs the reader in a step ordered ahead of the preflight that gates on it", () => {
    const steps = buildWorkflow().job.steps ?? [];
    const install = steps.findIndex((s) => s.id === "install-skopeo");
    expect(install, "no step with id install-skopeo").toBeGreaterThanOrEqual(0);
    expect(install, "an install ordered after the preflight cannot satisfy it").toBeLessThan(
      steps.findIndex((s) => s.id === "preflight"),
    );
    expect(steps[install].if, "a conditional install does not fail closed").toBeUndefined();
    expect(steps[install]["continue-on-error"]).toBeUndefined();
    expect(steps[install].run, "nothing carries the binary to the later steps").toMatch(
      />>\s*"\$GITHUB_PATH"/,
    );
  });

  it("sweeps the unpacked binary on the step that runs even when the build failed", () => {
    const { job } = buildWorkflow();
    const install = (job.steps ?? []).find((s) => s.id === "install-skopeo");
    const dirVar = /rm -rf "\$(\w+)"/.exec(install?.run ?? "");
    expect(dirVar, "the install step clears no job-env directory first").not.toBeNull();
    expect(job.env?.[dirVar![1]], `the job defines no ${dirVar![1]}`).toBeTruthy();
    const sweep = (job.steps ?? []).find((s) => s.if === "always()" && /^rm -rf/.test(s.run ?? ""));
    expect(sweep?.run, `the always() sweep does not remove $${dirVar![1]}`).toContain(
      `"$${dirVar![1]}"`,
    );
  });

  it("tries every mirror it lists, then names where to re-pin instead of failing blank", () => {
    const r = runInstall("exit 22");
    const listed = (
      (buildWorkflow().job.steps ?? []).find((s) => s.id === "install-skopeo")!.env?.SKOPEO_URLS ??
      ""
    )
      .split(/\s+/)
      .filter(Boolean);
    expect(listed.length, "a single mirror leaves the pin with no fallback").toBeGreaterThan(1);
    expect(r.urls, "the step gave up before trying every mirror").toHaveLength(listed.length);
    expect(r.status, "no mirror served the package and the step still passed").toBeGreaterThan(0);
    expect(r.out).toContain("Re-pin SKOPEO_DEB");
    expect(r.ghPath, "a failed install still put a directory on PATH").toBe("");
    expect(r.out, "the step reached the unpack with no package").not.toMatch(/dpkg-deb/);
  });

  it("refuses a download whose digest is not the pinned one", () => {
    const r = runInstall('printf tampered > "$out"');
    expect(r.status, "a package failing its checksum was accepted").toBeGreaterThan(0);
    expect(r.out, "the checksum was not what rejected it").toMatch(/FAILED|did NOT match/i);
    expect(r.ghPath, "a rejected package still went on PATH").toBe("");
    expect(r.out, "the step unpacked a package the checksum rejected").not.toMatch(/dpkg-deb/);
  });

  it("unpacks the package it checksummed, and puts that tree on PATH", () => {
    const body =
      (buildWorkflow().job.steps ?? []).find((s) => s.id === "install-skopeo")?.run ?? "";
    const unpack = /dpkg-deb -x "(\S+)" "(\S+)"/.exec(body);
    expect(unpack, "the step unpacks no package").not.toBeNull();
    const checked = /echo "\$\w+ {2}(\S+)" \| sha256sum -c/.exec(body);
    expect(checked, "the step checksums no package").not.toBeNull();
    expect(unpack![1], "the package unpacked is not the one checksummed").toBe(checked![1]);
    expect(body.indexOf("sha256sum -c"), "unpacked before the checksum ran").toBeLessThan(
      body.indexOf("dpkg-deb -x"),
    );
    const onPath = /echo "(\S+)" >> "\$GITHUB_PATH"/.exec(body);
    expect(onPath, "nothing is added to $GITHUB_PATH").not.toBeNull();
    expect(
      onPath![1].startsWith(`${unpack![2]}/`),
      `$GITHUB_PATH gets ${onPath![1]}, outside the ${unpack![2]} the package unpacked into`,
    ).toBe(true);
  });

  it("keeps the digest-read credential out of the build context and inside the sweep", () => {
    const wf = buildWorkflow();
    const steps = wf.job.steps ?? [];
    const build = steps.find((s) => s.id === "build");
    const cred = /^\$\{\{ env\.(\w+) \}\}$/.exec(build?.env?.TMPDIR ?? "");
    expect(cred, "the build step points TMPDIR at no job-env directory").not.toBeNull();
    const credDir = wf.job.env?.[cred![1]];
    expect(credDir, `the job defines no ${cred![1]}`).toBeTruthy();
    // mktemp -d fails if TMPDIR is absent, dropping the digest read to anonymous.
    const body = build!.run ?? "";
    const made = body.search(new RegExp(`install -d -m 700 "\\$\\{?${cred![1]}\\}?"`));
    const cleared = body.search(new RegExp(`rm -rf "\\$\\{?${cred![1]}\\}?"`));
    expect(made, `the build step does not create $${cred![1]} at mode 700`).toBeGreaterThanOrEqual(
      0,
    );
    expect(cleared, `the build step does not clear $${cred![1]} first`).toBeGreaterThanOrEqual(0);
    expect(cleared, `$${cred![1]} is created before the clear`).toBeLessThan(made);

    const ctx = /cd "\$\{?(\w+)\}?\//.exec(build!.run ?? "");
    expect(ctx, "the build step cds into no job-env directory").not.toBeNull();
    const ctxDir = wf.job.env?.[ctx![1]];
    expect(ctxDir, `the job defines no ${ctx![1]}`).toBeTruthy();
    const expand = (s: string) =>
      s.replace(/\$\{\{\s*([^}]+?)\s*\}\}/g, (_m, e: string) => `<${e}>`);
    expect(
      relative(expand(ctxDir!), expand(credDir!)).startsWith(".."),
      "the credential lands inside the podman build context",
    ).toBe(true);

    const cleanup = steps.find((s) => s.if === "always()" && /^\s*rm -rf /.test(s.run ?? ""));
    expect(cleanup, "no always() step removes anything").toBeDefined();
    for (const v of [cred![1], ctx![1]]) {
      expect(cleanup!.run, `the cleanup leaves $${v} behind`).toContain(`"$${v}"`);
    }
  });

  it("exits 130 with no ref to pin when interrupted during the digest read", async () => {
    const { status, out, authdir, pinnedRef } = await runBuildCancelled("login");
    expect(authdir, "the stubbed login never wrote an authfile").not.toBeNull();
    expect(status, "an interrupted build did not exit 130").toBe(130);
    expect(out, "an interrupted build printed the pin-it-by-hand guidance").not.toContain(
      "Could not read the registry digest",
    );
    expect(pinnedRef, "an interrupted build reported a reference to pin").toBeNull();
    expect(existsSync(authdir!), "the credential outlived the interrupt").toBe(false);
  }, 20000);

  it("exits 130 with no digest read when interrupted during the push", async () => {
    const { status, calls, pinnedRef } = await runBuildCancelled("push");
    expect(
      calls.filter((c) => c.startsWith("push ")),
      "the stubbed push never ran, so nothing was interrupted",
    ).not.toHaveLength(0);
    expect(status, "an interrupted push did not exit 130").toBe(130);
    expect(
      calls.filter((c) => c.startsWith("skopeo ")),
      "an interrupted push went on to read a digest",
    ).toHaveLength(0);
    expect(pinnedRef, "an interrupted push reported a reference to pin").toBeNull();
  }, 20000);

  it("creates that authfile under TMPDIR rather than a hardcoded /tmp", () => {
    const base = mkdtempSync(resolve(tmpdir(), "authbase-"));
    try {
      const { calls } = runBuild({
        PUSH_LATEST: "0",
        REGISTRY_USER: "u",
        REGISTRY_PASS: "p",
        TMPDIR: base,
      });
      const login = calls.find((c) => c.startsWith("skopeo login"));
      const authfile = /--authfile (\S+)/.exec(login ?? "");
      expect(authfile, "no skopeo login wrote an authfile").not.toBeNull();
      expect(authfile![1].startsWith(`${base}/`), `${authfile![1]} is outside TMPDIR`).toBe(true);
    } finally {
      rmSync(base, { recursive: true, force: true });
    }
  });
});

const NEW_PIN = pinFor("beefca7");
const REPIN_REL = relative(REPO_ROOT, REPIN_SH);

function runRepin(dir: string, ref: string): { status: number | null; out: string } {
  const r = spawnSync("bash", [resolve(dir, "repin.sh"), ref], { encoding: "utf8" });
  return { status: r.status, out: `${r.stdout}${r.stderr}` };
}

function agreement(dir: string): { status: number | null; out: string } {
  const r = spawnSync("bash", [resolve(dir, "prepull-image.sh"), "--check-agreement"], {
    encoding: "utf8",
  });
  return { status: r.status, out: `${r.stdout}${r.stderr}` };
}

function snapshot(dir: string): string[] {
  return manifestFiles(dir).map((f) => readFileSync(resolve(dir, f), "utf8"));
}

function jobOf(name: string): BuildJob & Record<string, unknown> {
  const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
    jobs?: Record<string, BuildJob & Record<string, unknown>>;
  };
  const job = wf.jobs?.[name];
  expect(job, `the workflow has no ${name} job`).toBeDefined();
  return job!;
}

/** Runs one workflow step's `run:` body verbatim, so its own failure paths execute. */
function runStep(
  job: string,
  match: (s: WorkflowStep) => boolean,
  env: Record<string, string>,
  cwd?: string,
): { status: number | null; out: string } {
  const step = (jobOf(job).steps ?? []).find(match);
  expect(step, `no matching step in ${job}`).toBeDefined();
  const dir = mkdtempSync(resolve(tmpdir(), "step-"));
  try {
    const body = resolve(dir, "step");
    writeFileSync(body, step!.run ?? "");
    const shell = (buildWorkflow().defaults?.run?.shell ?? "bash {0}").split(/\s+/);
    const argv = shell.map((w) => (w === "{0}" ? body : w));
    const r = spawnSync(argv[0], argv.slice(1), {
      encoding: "utf8",
      cwd: cwd ?? dir,
      env: { ...process.env, ...env },
    });
    return { status: r.status, out: `${r.stdout}${r.stderr}` };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

describe("one-command re-pin (#2345)", () => {
  it("moves every manifest that runs the image onto one reference in one command", () => {
    const dir = manifestCopy();
    const before = botImageRefs(dir);
    expect(before.length).toBeGreaterThan(2);
    expect(runRepin(dir, NEW_PIN).status).toBe(0);
    const after = botImageRefs(dir);
    expect(after.map((r) => `${r.file}:${r.container}`)).toEqual(
      before.map((r) => `${r.file}:${r.container}`),
    );
    expect([...new Set(after.map((r) => r.image))]).toEqual([NEW_PIN]);
    expect(agreement(dir).status).toBe(0);
  });

  it("repairs a partial re-pin instead of leaving one manifest behind", () => {
    const dir = manifestCopy();
    const partial = resolve(dir, "conductor-job.yaml");
    writeFileSync(partial, readFileSync(partial, "utf8").replace(agreedImage(), NEW_PIN));
    expect(agreement(dir).out).toMatch(/disagree on the bots-app image/);
    expect(runRepin(dir, NEW_PIN).status).toBe(0);
    expect([...new Set(botImageRefs(dir).map((r) => r.image))]).toEqual([NEW_PIN]);
    expect(agreement(dir).status).toBe(0);
  });

  it("is idempotent, so a re-run of the build cannot churn the manifests", () => {
    const dir = manifestCopy();
    expect(runRepin(dir, NEW_PIN).status).toBe(0);
    const once = snapshot(dir);
    const again = runRepin(dir, NEW_PIN);
    expect(again.status).toBe(0);
    expect(again.out).toContain("already at");
    expect(snapshot(dir)).toEqual(once);
  });

  it.each([
    ["", /usage:/],
    [NEW_PIN.split("@")[0], /carries no @sha256: digest/],
    [`${NEW_PIN.split("@")[0]}@sha256:abc`, /not a well-formed sha256 digest/],
    [NEW_PIN.replace("hclcr.io", "other.io"), /the fleet would never see it/],
    [NEW_PIN.replace(/:0\.1\.0-\d+-[0-9a-f]+@/, ":latest@"), /cannot read a commit from the tag/],
    [`${NEW_PIN}|e#`, /illegal character in the reference/],
  ])("refuses the reference %s and writes nothing", (ref, why) => {
    const dir = manifestCopy();
    const before = snapshot(dir);
    const r = runRepin(dir, ref);
    expect(r.status).not.toBe(0);
    expect(r.out).toMatch(why);
    expect(snapshot(dir)).toEqual(before);
  });

  it("refuses to report success when no manifest carried the pin", () => {
    const dir = manifestCopy();
    for (const f of manifestFiles(dir)) {
      const p = resolve(dir, f);
      writeFileSync(
        p,
        readFileSync(p, "utf8")
          .split("\n")
          .filter((l) => !new RegExp(`^\\s*-?\\s*image:\\s*\\S*${BOT_IMAGE_REPO}`).test(l))
          .join("\n"),
      );
    }
    expect(runRepin(dir, NEW_PIN).status).not.toBe(0);
  });

  it("refuses when a pin line the rewrite could not match is left behind", () => {
    const dir = manifestCopy();
    const sts = resolve(dir, "statefulset.yaml");
    writeFileSync(sts, readFileSync(sts, "utf8").replace(agreedImage(), `${agreedImage()} # ci`));
    const before = snapshot(dir);
    const r = runRepin(dir, NEW_PIN);
    expect(r.status).not.toBe(0);
    expect(r.out).toMatch(/disagree on the bots-app image/);
    expect(snapshot(dir), "a rejected re-pin left the manifests half-rewritten").toEqual(before);
  });

  it("satisfies the drift gate with the pin it writes, with no manifest edited by hand", () => {
    const { commit, run } = gitFixture();
    expect(run("--check-source-drift")).toContain(`matches the tree at ${commit}`);
    expect(run("run")).toContain("every schedulable node is warm");
  });

  it("is committed executable, or the command the build prints cannot run it", () => {
    const mode = execFileSync("git", ["-C", REPO_ROOT, "ls-files", "-s", "--", REPIN_REL], {
      encoding: "utf8",
    });
    expect(mode.startsWith("100755"), `${REPIN_REL} is recorded as ${mode.split(" ")[0]}`).toBe(
      true,
    );
  });

  it("keeps the re-pin out of the trigger, because a later matching path wins", () => {
    const paths = workflowWatchedPaths();
    const include = paths.indexOf("e2e/**");
    const exclude = paths.indexOf(`!${REPIN_REL.replace(/\/[^/]+$/, "/**")}`);
    expect(include, "the workflow no longer includes e2e/**").toBeGreaterThanOrEqual(0);
    expect(exclude, "an exclusion ahead of the include is nullified").toBeGreaterThan(include);
  });

  it("points a hand build at the script rather than at three files", () => {
    const { out, pinnedRef } = runBuild({ PUSH_LATEST: "0" });
    expect(pinnedRef).not.toBeNull();
    expect(out).toContain(`./k8s/repin.sh ${pinnedRef}`);
  });

  it("prints a summary command an operator can paste", () => {
    const summary = resolve(mkdtempSync(resolve(tmpdir(), "ghs-")), "summary");
    const r = runStep("build", (s) => /GITHUB_STEP_SUMMARY/.test(s.run ?? ""), {
      PINNED_REF: NEW_PIN,
      GITHUB_STEP_SUMMARY: summary,
    });
    expect(r.status).toBe(0);
    expect(readFileSync(summary, "utf8")).toContain(`./${REPIN_REL} ${NEW_PIN}`);
  });
});

const PIN_BASE = "PR-staging";
const PIN_BRANCH = `bots-image-pin/${PIN_BASE}`;
const REPIN_PR_SH = resolve(K8S, "repin-pr.sh");
const REPIN_PR_REL = relative(REPO_ROOT, REPIN_PR_SH);
const PIN_A = pinFor("beefca7");
const PIN_B = pinFor("dedbeef").replace(/sha256:a+$/, `sha256:${"b".repeat(64)}`);

interface PinRepo {
  work: string;
  origin: string;
  api: string;
  bin: string;
}

/** Responses come from resp.<call-number>, first line the HTTP status. */
const CURL_API_STUB = (api: string): string =>
  [
    "#!/bin/sh",
    `D='${api}'`,
    'n=$(cat "$D/n" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" >"$D/n"',
    'out=""; prev=""; url=""; method=GET; data=""',
    'for a in "$@"; do',
    '  case "$prev" in -o) out="$a" ;; -X) method="$a" ;; -d) data="$a" ;; esac',
    '  case "$a" in http*) url="$a" ;; esac',
    '  prev="$a"',
    "done",
    `printf '%s %s %s\\n' "$method" "$url" "$data" >>"$D/calls.log"`,
    'f="$D/resp.$n"',
    '[ -f "$f" ] || f="$D/resp.default"',
    'tail -n +2 "$f" >"$out"',
    `printf '%s' "$(head -1 "$f")"`,
  ].join("\n");

function reply(repo: PinRepo, n: number | "default", code: number, body: string): void {
  writeFileSync(resolve(repo.api, `resp.${n}`), `${code}\n${body}\n`);
}

function resetApi(repo: PinRepo): void {
  for (const f of readdirSync(repo.api)) rmSync(resolve(repo.api, f), { force: true });
  reply(repo, "default", 200, "[]");
}

/** A real remote that refuses a force, as this repo's pre-receive hook does. */
function pinRepo(): PinRepo {
  const root = mkdtempSync(resolve(tmpdir(), "repin-pr-"));
  const repo: PinRepo = {
    work: resolve(root, "work"),
    origin: resolve(root, "origin.git"),
    api: resolve(root, "api"),
    bin: resolve(root, "bin"),
  };
  mkdirSync(repo.api);
  mkdirSync(repo.bin);
  execFileSync("git", ["init", "--quiet", "--bare", repo.origin]);
  git(repo.origin, "config", "receive.denyNonFastForwards", "true");
  const k8s = resolve(repo.work, "e2e", "bots-app", "k8s");
  mkdirSync(k8s, { recursive: true });
  for (const f of [...manifestFiles(), TEMPLATE, "prepull-image.sh", "repin.sh", "repin-pr.sh"]) {
    copyFileSync(resolve(K8S, f), resolve(k8s, f));
    if (f.endsWith(".sh")) chmodSync(resolve(k8s, f), 0o755);
  }
  git(repo.work, "init", "--quiet", "-b", PIN_BASE);
  git(repo.work, "add", "-A");
  git(repo.work, "commit", "--quiet", "-m", "base");
  git(repo.work, "remote", "add", "origin", repo.origin);
  git(repo.work, "push", "--quiet", "origin", PIN_BASE);
  writeFileSync(resolve(repo.bin, "curl"), CURL_API_STUB(repo.api), { mode: 0o755 });
  resetApi(repo);
  return repo;
}

interface PinRun {
  status: number | null;
  out: string;
  outputs: Record<string, string>;
  calls: string[];
  summary: string;
}

/** Back to the base branch, as the workflow's own fresh clone would be. */
function rewind(repo: PinRepo): void {
  git(repo.work, "checkout", "--quiet", PIN_BASE);
  git(repo.work, "reset", "--hard", "--quiet", `origin/${PIN_BASE}`);
}

function runRepinPr(repo: PinRepo, ref: string, extra: Record<string, string> = {}): PinRun {
  const outputs = resolve(repo.api, "..", "outputs");
  const summary = resolve(repo.api, "..", "summary");
  writeFileSync(outputs, "");
  writeFileSync(summary, "");
  const r = spawnSync("bash", [resolve(repo.work, "e2e/bots-app/k8s/repin-pr.sh")], {
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${repo.bin}:${process.env.PATH ?? ""}`,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      PINNED_REF: ref,
      PIN_BRANCH,
      BASE_BRANCH: PIN_BASE,
      API_URL: "https://example.test/api/v3",
      SERVER_URL: "https://example.test",
      REPO_SLUG: "labs-projects/videocall",
      RUN_URL: "https://example.test/run/1",
      PR_TOKEN: "tok",
      GITHUB_OUTPUT: outputs,
      GITHUB_STEP_SUMMARY: summary,
      ...extra,
    },
  });
  const log = resolve(repo.api, "calls.log");
  return {
    status: r.status,
    out: `${r.stdout}${r.stderr}`,
    outputs: Object.fromEntries(
      readFileSync(outputs, "utf8")
        .split("\n")
        .filter(Boolean)
        .map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]),
    ),
    calls: existsSync(log) ? readFileSync(log, "utf8").split("\n").filter(Boolean) : [],
    summary: readFileSync(summary, "utf8"),
  };
}

function pinTip(repo: PinRepo): string | null {
  try {
    return git(repo.origin, "rev-parse", `refs/heads/${PIN_BRANCH}`).trim();
  } catch {
    return null;
  }
}

function pinnedFiles(repo: PinRepo): string[] {
  for (const b of [PIN_BASE, PIN_BRANCH]) {
    git(repo.work, "fetch", "--quiet", "origin", `+refs/heads/${b}:refs/remotes/origin/${b}`);
  }
  return git(repo.work, "diff", "--name-only", `origin/${PIN_BASE}`, `origin/${PIN_BRANCH}`)
    .split("\n")
    .filter(Boolean);
}

/** An open PR for the head branch, then a successful update of it. */
function replyWithOpenPr(repo: PinRepo, number = 4242): void {
  reply(repo, 1, 200, `[{"number": ${number}}]`);
  reply(repo, 2, 200, `{"number": ${number}}`);
}

// Each case drives real git against a real remote, so the 5s default is tight.
describe("automated re-pin PR (#2400)", { timeout: 30000 }, () => {
  it("commits the pin on its own branch and opens one PR against the base branch", () => {
    const repo = pinRepo();
    const base = git(repo.origin, "rev-parse", `refs/heads/${PIN_BASE}`).trim();
    reply(repo, 2, 201, '{"number": 4242}');
    const r = runRepinPr(repo, PIN_A);
    expect(r.status, r.out).toBe(0);
    expect(r.outputs.action).toBe("opened");
    expect(r.outputs.pr_url).toBe("https://example.test/labs-projects/videocall/pull/4242");
    expect(pinTip(repo)).not.toBeNull();
    expect(
      git(repo.origin, "rev-parse", `refs/heads/${PIN_BASE}`).trim(),
      "the pin was committed onto the base branch itself",
    ).toBe(base);
    expect(pinnedFiles(repo)).toEqual([
      "e2e/bots-app/k8s/bot-pod.yaml",
      "e2e/bots-app/k8s/conductor-job.yaml",
      "e2e/bots-app/k8s/statefulset.yaml",
    ]);
    const post = r.calls.find((c) => c.startsWith("POST "));
    expect(post, "no PR was created").toBeDefined();
    expect(post).toContain(PIN_A);
    expect(post).toContain(`"base":"${PIN_BASE}"`);
    expect(post).toContain(`"head":"${PIN_BRANCH}"`);
  });

  it("advances the pin branch by fast-forward, which a remote refusing a force still takes", () => {
    const repo = pinRepo();
    reply(repo, 2, 201, '{"number": 4242}');
    expect(runRepinPr(repo, PIN_A).status).toBe(0);
    const first = pinTip(repo)!;
    rewind(repo);
    resetApi(repo);
    replyWithOpenPr(repo);
    const r = runRepinPr(repo, PIN_B);
    expect(r.status, r.out).toBe(0);
    const second = pinTip(repo)!;
    expect(second, "the pin branch did not advance").not.toBe(first);
    expect(
      () => git(repo.work, "merge-base", "--is-ancestor", first, second),
      "the new tip does not descend from the old, so only a force could publish it",
    ).not.toThrow();
    expect(pinnedFiles(repo)).toHaveLength(3);
    expect(
      git(repo.origin, "show", `refs/heads/${PIN_BRANCH}:e2e/bots-app/k8s/statefulset.yaml`),
    ).toContain(PIN_B);
  });

  it("updates the open PR instead of opening a second one, so its body cannot name a superseded digest", () => {
    const repo = pinRepo();
    reply(repo, 2, 201, '{"number": 4242}');
    runRepinPr(repo, PIN_A);
    rewind(repo);
    resetApi(repo);
    replyWithOpenPr(repo);
    const r = runRepinPr(repo, PIN_B);
    expect(r.outputs.action).toBe("updated");
    expect(r.calls.filter((c) => c.startsWith("POST "))).toHaveLength(0);
    const patch = r.calls.find((c) => c.startsWith("PATCH "));
    expect(patch).toContain("/pulls/4242");
    expect(patch).toContain(PIN_B);
    expect(patch, "the body still names the digest it no longer pins").not.toContain(PIN_A);
  });

  it("leaves the pin branch where it is when the same reference is built again", () => {
    const repo = pinRepo();
    reply(repo, 2, 201, '{"number": 4242}');
    runRepinPr(repo, PIN_A);
    const first = pinTip(repo)!;
    rewind(repo);
    resetApi(repo);
    replyWithOpenPr(repo);
    const r = runRepinPr(repo, PIN_A);
    expect(r.status, r.out).toBe(0);
    expect(pinTip(repo), "a re-run churned the branch").toBe(first);
    expect(r.calls.filter((c) => c.startsWith("POST "))).toHaveLength(0);
  });

  it("writes nothing and calls no API when the base branch already pins the built reference", () => {
    const repo = pinRepo();
    const k8s = resolve(repo.work, "e2e", "bots-app", "k8s");
    execFileSync("bash", [resolve(k8s, "repin.sh"), PIN_A], { encoding: "utf8" });
    git(repo.work, "commit", "--quiet", "-a", "-m", "pin");
    git(repo.work, "push", "--quiet", "origin", PIN_BASE);
    const r = runRepinPr(repo, PIN_A);
    expect(r.status, r.out).toBe(0);
    expect(r.outputs.action).toBe("already-pinned");
    expect(r.calls, "an up-to-date pin still called the API").toEqual([]);
    expect(pinTip(repo), "an up-to-date pin still created a branch").toBeNull();
  });

  it("refuses to open a PR when the build resolved no reference to pin", () => {
    const repo = pinRepo();
    const r = runRepinPr(repo, "");
    expect(r.status).not.toBe(0);
    expect(r.out).toMatch(/no reference to pin/);
    expect(pinTip(repo)).toBeNull();
    expect(r.calls).toEqual([]);
  });

  it("pushes nothing when the reference names another repository", () => {
    const repo = pinRepo();
    const r = runRepinPr(repo, PIN_A.replace("hclcr.io", "other.io"));
    expect(r.status).not.toBe(0);
    expect(pinTip(repo)).toBeNull();
    expect(r.calls).toEqual([]);
  });

  it("refuses when the re-pin did not leave the manifests naming the built reference", () => {
    const repo = pinRepo();
    writeFileSync(resolve(repo.work, "e2e/bots-app/k8s/repin.sh"), "#!/bin/sh\nexit 0\n", {
      mode: 0o755,
    });
    const r = runRepinPr(repo, PIN_A);
    expect(r.status).not.toBe(0);
    expect(r.out).toMatch(/the manifests name .*, not /);
    expect(pinTip(repo)).toBeNull();
  });

  it("pushes the pin and points at a one-click compare when no PR credential is configured", () => {
    const repo = pinRepo();
    const r = runRepinPr(repo, PIN_A, { PR_TOKEN: "" });
    expect(r.status, r.out).toBe(0);
    expect(r.outputs.action).toBe("pushed");
    expect(pinTip(repo), "the pin was not even pushed").not.toBeNull();
    expect(r.calls, "a run with no credential still called the API").toEqual([]);
    expect(r.out).toMatch(/^::warning::/m);
    expect(r.summary).toContain(
      `https://example.test/labs-projects/videocall/compare/${PIN_BASE}...${PIN_BRANCH}?expand=1`,
    );
    expect(r.summary).toContain(PIN_A);
  });

  it("adopts the open PR when create is refused, instead of wedging on a 422 forever", () => {
    const repo = pinRepo();
    reply(repo, 1, 200, "[]");
    reply(repo, 2, 422, '{"message":"A pull request already exists"}');
    reply(repo, 3, 200, '[{"number": 99}]');
    reply(repo, 4, 200, '{"number": 99}');
    const r = runRepinPr(repo, PIN_A);
    expect(r.status, r.out).toBe(0);
    expect(r.outputs.action).toBe("updated");
    expect(r.outputs.pr_number).toBe("99");
    expect(r.calls.filter((c) => c.startsWith("PATCH "))).toHaveLength(1);
  });

  it("fails the job when the PR could not be opened at all, rather than reporting a pin it never announced", () => {
    const repo = pinRepo();
    reply(repo, 2, 500, '{"message":"boom"}');
    const r = runRepinPr(repo, PIN_A);
    expect(r.status).not.toBe(0);
    expect(r.out).toMatch(/opening the re-pin PR failed \(HTTP 500\)/);
  });

  it("supersedes whatever the pin branch carried, so the PR diff is the pin and nothing else", () => {
    const repo = pinRepo();
    reply(repo, 2, 201, '{"number": 4242}');
    runRepinPr(repo, PIN_A);
    git(repo.work, "checkout", "--quiet", PIN_BRANCH);
    writeFileSync(resolve(repo.work, "stowaway.txt"), "not a pin\n");
    git(repo.work, "add", "-A");
    git(repo.work, "commit", "--quiet", "-m", "stowaway");
    git(repo.work, "push", "--quiet", "origin", PIN_BRANCH);
    rewind(repo);
    resetApi(repo);
    replyWithOpenPr(repo);
    const r = runRepinPr(repo, PIN_B);
    expect(r.status, r.out).toBe(0);
    expect(pinnedFiles(repo), "the PR would merge a file that is not the pin").toEqual([
      "e2e/bots-app/k8s/bot-pod.yaml",
      "e2e/bots-app/k8s/conductor-job.yaml",
      "e2e/bots-app/k8s/statefulset.yaml",
    ]);
  });
});

describe("the workflow half of the automated re-pin (#2400)", () => {
  const repinJob = () => jobOf("repin");
  const repinStep = () => (repinJob().steps ?? []).find((s) => s.id === "repin");

  it("hands the built reference to the re-pin, and fails there rather than skipping when it is empty", () => {
    const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
      jobs?: Record<string, { outputs?: Record<string, string> }>;
    };
    expect(wf.jobs?.build?.outputs?.pinned_ref).toBe("${{ steps.build.outputs.pinned_ref }}");
    expect(repinStep()?.env?.PINNED_REF).toBe("${{ needs.build.outputs.pinned_ref }}");
    expect(
      String(repinJob().if ?? ""),
      "a job that skips on an empty ref succeeds having pinned nothing",
    ).not.toContain("pinned_ref");
  });

  it("opens the pin PR against one branch only, on a dispatch as much as on a push", () => {
    const on = (parseYaml(readFileSync(WORKFLOW, "utf8")) as Record<string, unknown>).on as {
      push?: { branches?: string[] };
    };
    expect(on?.push?.branches, "the guarded branch never builds, so the job is inert").toContain(
      PIN_BASE,
    );
    expect(repinJob().if, "workflow_dispatch carries no branch filter of its own").toBe(
      `github.ref_name == '${PIN_BASE}'`,
    );
    expect(repinStep()?.env?.BASE_BRANCH).toBe("${{ github.ref_name }}");
    expect(repinStep()?.env?.PIN_BRANCH).toBe("bots-image-pin/${{ github.ref_name }}");
  });

  it("pins the commit the image was built from, not a branch head that moved during the build", () => {
    const step = (repinJob().steps ?? []).find((s) => /git clone/.test(s.run ?? ""));
    expect(step?.run, "no step checks out anything").toBeDefined();
    expect(step!.run).toContain("git checkout --quiet ${{ github.sha }}");
    expect(step!.run).toMatch(/test "\$\(git rev-parse HEAD\)" = "\$\{\{ github\.sha \}\}"/);
  });

  it("sweeps the build's push key on the step that runs even when the build failed", () => {
    const { job } = buildWorkflow();
    const setup = (job.steps ?? []).find((s) => /podman/.test(s.run ?? ""));
    const key = /install -m 600 \/dev\/null "([^"]+)"/.exec(setup?.run ?? "");
    expect(key, "the setup step installs no key file").not.toBeNull();
    const sweep = (job.steps ?? []).find((s) => s.if === "always()" && /^rm -rf/.test(s.run ?? ""));
    expect(sweep?.run, "the push key outlives the build, beside a write-scoped job").toContain(
      key![1],
    );
  });

  it("keeps the write scopes on the re-pin alone, and never forces the push", () => {
    const wf = parseYaml(readFileSync(WORKFLOW, "utf8")) as {
      permissions?: Record<string, string>;
      jobs?: Record<string, { permissions?: Record<string, string> }>;
    };
    expect(wf.permissions).toEqual({ contents: "read" });
    expect(wf.jobs?.build?.permissions, "the build job holds a write scope").toBeUndefined();
    expect(wf.jobs?.repin?.permissions).toEqual({
      contents: "write",
      "pull-requests": "write",
    });
    const pushes = readFileSync(REPIN_PR_SH, "utf8")
      .split("\n")
      .filter((l) => /\bpush\b/.test(l));
    expect(pushes.length, "the re-pin pushes nothing").toBeGreaterThan(0);
    for (const line of pushes) {
      expect(line, "a forced push is rejected repo-wide by a pre-receive hook").not.toMatch(
        /--force|\s-f\b|"\+/,
      );
    }
  });

  it("runs the re-pin only after the build has swept its credentials", () => {
    expect(repinJob().needs).toBe("build");
  });

  it("stops before the clone when a tool it needs is missing, and says so when the credential is", () => {
    const steps = repinJob().steps ?? [];
    const pre = steps.findIndex((s) => s.id === "repin-preflight");
    expect(pre, "no step with id repin-preflight").toBeGreaterThanOrEqual(0);
    expect(pre, "a preflight ordered after the clone cannot gate it").toBeLessThan(
      steps.findIndex((s) => /git clone/.test(s.run ?? "")),
    );
    expect(steps[pre].if, "a conditional preflight does not fail closed").toBeUndefined();
    expect(steps[pre]["continue-on-error"]).toBeUndefined();

    const dir = mkdtempSync(resolve(tmpdir(), "repin-preflight-"));
    try {
      const link = (tool: string) => {
        const real = spawnSync("sh", ["-c", `command -v ${tool}`], {
          encoding: "utf8",
        }).stdout.trim();
        expect(real, `${tool} is not on PATH`).not.toBe("");
        symlinkSync(real, resolve(dir, tool));
      };
      link("bash");
      const run = (token: string) =>
        runStep("repin", (s) => s.id === "repin-preflight", { PATH: dir, PR_TOKEN: token });
      expect(
        run("x").status,
        "the preflight passed with none of its tools on PATH",
      ).toBeGreaterThan(0);
      for (const tool of ["git", "curl", "jq"]) link(tool);
      const ok = run("x");
      expect(ok.status, ok.out).toBe(0);
      expect(ok.out, "a configured credential still warned").not.toMatch(/::warning::/);
      const none = run("");
      expect(none.status, none.out).toBe(0);
      expect(none.out, "an unset credential passed silently").toMatch(
        /::warning::.*BOTS_REPIN_TOKEN/,
      );
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("re-runs this suite when the automation it locks changes", () => {
    const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
    const on = wf.on as { pull_request?: { paths?: string[] } };
    expect(REPIN_PR_REL.startsWith("e2e/"), `${REPIN_PR_REL} is outside the watched tree`).toBe(
      true,
    );
    expect(on?.pull_request?.paths).toContain("e2e/**");
  });
});
