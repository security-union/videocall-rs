import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

/**
 * Locks the netem-preload DaemonSet against the entrypoint that depends on it,
 * and EXECUTES its scripts with `nsenter` stubbed. Both sides are read from the
 * real files, so a shaping command whose module is not preloaded fails here.
 */
const DS_PATH = fileURLToPath(new URL("../../k8s/netem-preload-daemonset.yaml", import.meta.url));
const ENTRYPOINT_PATH = fileURLToPath(new URL("../../docker-entrypoint.sh", import.meta.url));

interface DaemonSetContainer {
  args: string[];
  env: { name: string; value?: string }[];
  readinessProbe: { exec: { command: string[] } };
}

const container = (): DaemonSetContainer => {
  const doc = parseYaml(readFileSync(DS_PATH, "utf8")) as {
    spec: { template: { spec: { containers: DaemonSetContainer[] } } };
  };
  return doc.spec.template.spec.containers[0];
};

const startupScript = (): string => container().args[0];
const probeScript = (): string => {
  const cmd = container().readinessProbe.exec.command;
  return cmd[cmd.length - 1];
};
const moduleList = (): string[] => {
  const m = /MODULES="([^"]*)"/.exec(startupScript());
  expect(m, "the startup script must declare MODULES").not.toBeNull();
  return m![1].split(/\s+/).filter(Boolean);
};

/** Runs a DaemonSet script with `nsenter` stubbed — no host namespace entered. */
function runScript(
  script: string,
  opts: { moduleFile?: string; builtinRc?: number; modprobeRc?: number; timeoutSecs?: number } = {},
): { status: number | null; stdout: string; stderr: string; moduleFile: string } {
  const workdir = mkdtempSync(join(tmpdir(), "netem-preload-"));
  const stubBin = join(workdir, "bin");
  const nsenter = join(stubBin, "nsenter");
  mkdirSync(stubBin, { recursive: true });
  writeFileSync(
    nsenter,
    [
      "#!/usr/bin/env bash",
      `[[ "$*" == *" modprobe "* ]] && exit ${opts.modprobeRc ?? 0}`,
      `[[ "$*" == *" grep "* ]] && exit ${opts.builtinRc ?? 0}`,
      "exit 0",
      "",
    ].join("\n"),
  );
  chmodSync(nsenter, 0o755);
  const moduleFile = opts.moduleFile ?? join(workdir, "netem-modules");
  const res = spawnSync("timeout", [String(opts.timeoutSecs ?? 2), "/bin/sh", "-c", script], {
    encoding: "utf8",
    env: {
      PATH: `${stubBin}:${process.env.PATH ?? "/usr/bin:/bin"}`,
      MODULE_FILE: moduleFile,
      NODE_NAME: "test-node",
    },
  });
  let written: string;
  try {
    written = readFileSync(moduleFile, "utf8");
  } catch {
    written = "";
  }
  rmSync(workdir, { recursive: true, force: true });
  return {
    status: res.status,
    stdout: res.stdout ?? "",
    stderr: res.stderr ?? "",
    moduleFile: written,
  };
}

describe("netem-preload DaemonSet (#2072/#2353)", () => {
  it("preloads every module the entrypoint's shaping commands need", () => {
    const entrypoint = readFileSync(ENTRYPOINT_PATH, "utf8");
    const required: [RegExp, string][] = [
      [/root netem/, "sch_netem"],
      [/type ifb/, "ifb"],
      [/action mirred/, "act_mirred"],
      [/u32 match u32/, "cls_u32"],
      [/handle ffff: ingress/, "sch_ingress"],
    ];
    const modules = moduleList();
    const used = required.filter(([usage]) => usage.test(entrypoint));
    expect(used.length, "the entrypoint must still issue shaping commands").toBe(required.length);
    for (const [usage, mod] of used) {
      expect(modules, `${usage.source} in docker-entrypoint.sh requires ${mod}`).toContain(mod);
    }
  });

  it("gates readiness on the SAME list startup wrote, not a second copy", () => {
    const probe = probeScript();
    for (const mod of moduleList()) {
      expect(probe, "the probe must not hardcode module names").not.toContain(mod);
    }
    expect(probe).toContain("MODULE_FILE");
    expect(container().env.map((e) => e.name)).toContain("MODULE_FILE");
  });

  it("publishes the module list and stays alive once every module is present", () => {
    const r = runScript(startupScript(), { builtinRc: 0 });
    // 124 = `timeout` killed it: a DaemonSet pod that exits 0 CrashLoops.
    expect(r.status).toBe(124);
    expect(r.stdout).toContain("all present");
    expect(r.moduleFile.split(/\s+/).filter(Boolean)).toEqual(moduleList());
  });

  it("exits non-zero when a module is neither loadable nor built in", () => {
    // A fake name, so the host's real /proc/modules cannot satisfy the check.
    const script = startupScript().replace(/MODULES="[^"]*"/, 'MODULES="bots_app_absent_mod"');
    const r = runScript(script, { builtinRc: 1, modprobeRc: 1 });
    expect(r.status).toBe(1);
    expect(r.stderr).toContain("neither loadable nor built in");
    expect(r.moduleFile).toBe("");
  });

  it("fails readiness on an unwritten module file instead of reporting READY", () => {
    const dir = mkdtempSync(join(tmpdir(), "netem-empty-"));
    const empty = join(dir, "modules");
    writeFileSync(empty, "");
    expect(runScript(probeScript(), { moduleFile: empty }).status).not.toBe(0);
    rmSync(dir, { recursive: true, force: true });
  });

  it("accepts a module compiled into the kernel, which never appears in /proc/modules", () => {
    const dir = mkdtempSync(join(tmpdir(), "netem-builtin-"));
    const file = join(dir, "modules");
    writeFileSync(file, "bots_app_absent_mod\n");
    expect(runScript(probeScript(), { moduleFile: file, builtinRc: 0 }).status).toBe(0);
    expect(runScript(probeScript(), { moduleFile: file, builtinRc: 1 }).status).not.toBe(0);
    rmSync(dir, { recursive: true, force: true });
  });
});
