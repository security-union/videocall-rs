import { afterEach, describe, expect, it, vi } from "vitest";

import {
  applyNetemAction,
  buildNetemClearArgs,
  buildNetemMirrorClearArgs,
  buildNetemMirrorInstallArgs,
  buildNetemProbeArgs,
  buildNetemShapeArgs,
  defaultNetemExec,
  ingressNetemParams,
  LOOPBACK_IFACES,
  NETEM_IFB_DEV,
  NETEM_IFB_TXQUEUELEN,
  NETEM_INGRESS_QDISC_MARKER,
  NETEM_MIRROR_ADD_STEP,
  NETEM_PARAM_KEYS,
  NETEM_PROFILES,
  NETEM_SETPRIV_DEFAULT,
  type NetemCommand,
  type NetemExec,
  NetemExecError,
  netemExecExitedNonZero,
  NetemStateError,
  NetemValidationError,
  netemSetprivPath,
  resolveNetemRequest,
  validateNetemParams,
} from "./netem";

/** A recording exec: captures every `tc`/`ip` invocation, never runs one. */
function recordingExec(stdout = ""): {
  exec: NetemExec;
  calls: Array<{ file: string; args: string[] }>;
} {
  const calls: Array<{ file: string; args: string[] }> = [];
  const exec: NetemExec = async (file, args) => {
    calls.push({ file, args });
    return { stdout, stderr: "" };
  };
  return { exec, calls };
}

function failingExec(msg: string, exitStatus: number | null = 2): NetemExec {
  return async () => {
    throw new NetemExecError(msg, exitStatus);
  };
}

/** Fails only the commands whose joined argv contains `needle`. */
function failingOnlyExec(
  needle: string,
  msg: string,
  exitStatus: number | null = 2,
  stdout = "",
): { exec: NetemExec; calls: Array<{ file: string; args: string[] }> } {
  const calls: Array<{ file: string; args: string[] }> = [];
  const exec: NetemExec = async (file, args) => {
    calls.push({ file, args });
    if ([file, ...args].join(" ").includes(needle)) throw new NetemExecError(msg, exitStatus);
    return { stdout, stderr: "" };
  };
  return { exec, calls };
}

const flat = (cmds: NetemCommand[]): string[][] => cmds.map((c) => [c.file, ...c.args]);

const SETPRIV_IP = [
  NETEM_SETPRIV_DEFAULT,
  "--inh-caps",
  "+net_admin",
  "--ambient-caps",
  "+net_admin",
  "--",
  "ip",
];
const ran = (cmds: NetemCommand[]): string[][] =>
  cmds.map((c) => (c.file === "ip" ? [...SETPRIV_IP, ...c.args] : ["tc", ...c.args]));

const ETH_NETEM = "qdisc netem 8001: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 2Mbit";
const ETH_HOOK = `${NETEM_INGRESS_QDISC_MARKER} ffff: parent ffff:fff1 ----------------`;
const IFB_NETEM = "qdisc netem 8002: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 4Mbit";

/** Answers `tc qdisc show dev <dev>` from `show`; fails any argv containing `fail.needle`. */
type PodFail = { needle: string; msg: string; status?: number | null };
function podExec(
  show: Record<string, string>,
  fail?: PodFail | PodFail[],
): { exec: NetemExec; calls: string[][] } {
  const calls: string[][] = [];
  const fails = fail === undefined ? [] : Array.isArray(fail) ? fail : [fail];
  const exec: NetemExec = async (file, args) => {
    const argv = [file, ...args];
    calls.push(argv);
    const f = fails.find((x) => argv.join(" ").includes(x.needle));
    if (f) throw new NetemExecError(f.msg, f.status === undefined ? 1 : f.status);
    if (file === "tc" && args[0] === "qdisc" && args[1] === "show") {
      return { stdout: show[args[3]] ?? "", stderr: "" };
    }
    return { stdout: "", stderr: "" };
  };
  return { exec, calls };
}

const BOTH_SHAPED = { eth0: `${ETH_NETEM}\n${ETH_HOOK}`, [NETEM_IFB_DEV]: IFB_NETEM };

describe("buildNetemShapeArgs", () => {
  it("builds a full delay+jitter+loss+rate command in netem grammar order", () => {
    const args = buildNetemShapeArgs("eth0", {
      delayMs: 150,
      jitterMs: 50,
      lossPct: 5,
      rateKbit: 800,
    });
    // Exact argv — order matters to netem: delay [jitter], loss, rate.
    // `replace` (not `add`) keeps re-application idempotent.
    expect(args).toEqual([
      "qdisc",
      "replace",
      "dev",
      "eth0",
      "root",
      "netem",
      "delay",
      "150ms",
      "50ms",
      "loss",
      "5%",
      "rate",
      "800kbit",
    ]);
  });

  it("omits sections whose params are absent (delay only)", () => {
    expect(buildNetemShapeArgs("eth0", { delayMs: 100 })).toEqual([
      "qdisc",
      "replace",
      "dev",
      "eth0",
      "root",
      "netem",
      "delay",
      "100ms",
    ]);
  });

  it("omits the jitter token when only delay is given", () => {
    const args = buildNetemShapeArgs("eth0", { delayMs: 100, lossPct: 2 });
    expect(args).toEqual([
      "qdisc",
      "replace",
      "dev",
      "eth0",
      "root",
      "netem",
      "delay",
      "100ms",
      "loss",
      "2%",
    ]);
  });

  it("appends `limit` last in the argv, after `rate`", () => {
    const args = buildNetemShapeArgs("eth0", {
      delayMs: 200,
      jitterMs: 40,
      lossPct: 3,
      rateKbit: 56,
      limitPkts: 10,
    });
    expect(args).toEqual([
      "qdisc",
      "replace",
      "dev",
      "eth0",
      "root",
      "netem",
      "delay",
      "200ms",
      "40ms",
      "loss",
      "3%",
      "rate",
      "56kbit",
      "limit",
      "10",
    ]);
    expect(args.slice(-2)).toEqual(["limit", "10"]);
    expect(args.indexOf("limit")).toBeGreaterThan(args.indexOf("rate"));
  });

  it("omits `limit` entirely when limitPkts is unset", () => {
    expect(buildNetemShapeArgs("eth0", { rateKbit: 56 })).not.toContain("limit");
  });

  it("emits `limit` without a rate", () => {
    expect(buildNetemShapeArgs("eth0", { lossPct: 1, limitPkts: 1 }).slice(-2)).toEqual([
      "limit",
      "1",
    ]);
  });

  it("carries every shipped profile's queue depth into the argv", () => {
    for (const [name, params] of Object.entries(NETEM_PROFILES)) {
      if (params === null) continue;
      expect(params.limitPkts, `${name} must budget a queue depth`).toBeGreaterThan(0);
      expect(buildNetemShapeArgs("eth0", params).slice(-2), name).toEqual([
        "limit",
        `${params.limitPkts}`,
      ]);
    }
  });

  it("honors a non-default interface name", () => {
    expect(buildNetemShapeArgs("wlan0", { lossPct: 1 })).toContain("wlan0");
  });

  // ── injection safety ──────────────────────────────────────────────
  it.each([
    "eth0; rm -rf /",
    "eth0 && reboot",
    "$(whoami)",
    "eth0|cat",
    "../../dev/null",
    "eth0\n",
    "", // empty
    "a".repeat(16), // exceeds IFNAMSIZ-1
  ])("rejects a shell-unsafe / invalid interface name %j", (iface) => {
    expect(() => buildNetemShapeArgs(iface, { lossPct: 1 })).toThrow(NetemValidationError);
  });

  // Literal, never [...LOOPBACK_IFACES] — cases derived from it vanish when it shrinks.
  it.each(["lo", "lo0"])("refuses to shape loopback %j (#2349)", (iface) => {
    expect(() => buildNetemShapeArgs(iface, { lossPct: 95 })).toThrow(/readinessProbe/);
  });
});

describe("buildNetemClearArgs", () => {
  it("builds `qdisc del dev <iface> root`", () => {
    expect(buildNetemClearArgs("eth0")).toEqual(["qdisc", "del", "dev", "eth0", "root"]);
  });

  it("rejects an unsafe interface name", () => {
    expect(() => buildNetemClearArgs("eth0; echo hi")).toThrow(NetemValidationError);
  });

  it.each(["lo", "lo0"])("refuses to clear loopback %j", (iface) => {
    expect(() => buildNetemClearArgs(iface)).toThrow(NetemValidationError);
  });

  it("guards exactly the loopback names these cases cover", () => {
    expect([...LOOPBACK_IFACES].sort()).toEqual(["lo", "lo0"]);
  });
});

describe("validateNetemParams", () => {
  it("accepts a valid subset", () => {
    expect(validateNetemParams({ delayMs: 100, lossPct: 2.5 })).toEqual({
      delayMs: 100,
      lossPct: 2.5,
    });
  });

  it("rejects jitter without delay (netem grammar)", () => {
    expect(() => validateNetemParams({ jitterMs: 20 })).toThrow(/requires "delayMs"/);
  });

  it("rejects an empty impairment set", () => {
    expect(() => validateNetemParams({})).toThrow(/at least one/);
  });

  it("rejects negative, non-finite, and out-of-range values", () => {
    expect(() => validateNetemParams({ delayMs: -1 })).toThrow(NetemValidationError);
    expect(() => validateNetemParams({ lossPct: 101 })).toThrow(NetemValidationError);
    expect(() => validateNetemParams({ delayMs: Number.POSITIVE_INFINITY })).toThrow(
      NetemValidationError,
    );
    expect(() => validateNetemParams({ delayMs: Number.NaN })).toThrow(NetemValidationError);
    expect(() => validateNetemParams({ rateKbit: 0 })).toThrow(/>= 8/);
  });

  it("caps lossPct below 100 and floors rateKbit at 8 (self-DoS guard)", () => {
    expect(validateNetemParams({ lossPct: 95 }).lossPct).toBe(95);
    expect(() => validateNetemParams({ lossPct: 96 })).toThrow(/<= 95/);
    expect(() => validateNetemParams({ lossPct: 100 })).toThrow(/<= 95/);
    expect(validateNetemParams({ rateKbit: 8 }).rateKbit).toBe(8);
    expect(() => validateNetemParams({ rateKbit: 7 })).toThrow(/>= 8/);
  });

  it("validates limitPkts and keeps it out of the impairment set", () => {
    expect(validateNetemParams({ rateKbit: 56, limitPkts: 10 })).toEqual({
      rateKbit: 56,
      limitPkts: 10,
    });
    expect(validateNetemParams({ lossPct: 1, limitPkts: 1 }).limitPkts).toBe(1);
    expect(validateNetemParams({ lossPct: 1, limitPkts: 100_000 }).limitPkts).toBe(100_000);
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: 0 })).toThrow(/integer >= 1/);
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: 1.5 })).toThrow(/integer >= 1/);
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: -1 })).toThrow(/"limitPkts" must be/);
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: 100_001 })).toThrow(/<= 100000/);
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: Number.NaN })).toThrow(
      /finite number/,
    );
    expect(() => validateNetemParams({ lossPct: 1, limitPkts: Number.POSITIVE_INFINITY })).toThrow(
      /finite number/,
    );
    // A depth alone shapes nothing, so it must not satisfy the "at least one" gate.
    expect(() => validateNetemParams({ limitPkts: 10 })).toThrow(/at least one/);
  });

  it("accepts every shipped profile's params unchanged, both directions", () => {
    for (const [name, params] of Object.entries(NETEM_PROFILES)) {
      if (params === null) continue;
      expect(params.downlinkRateKbit, `${name} must carry a downlink rate`).toBeGreaterThan(0);
      expect(params.ingressLimitPkts, `${name} must carry an ingress depth`).toBeGreaterThan(0);
      expect(validateNetemParams({ ...params }), name).toEqual(params);
    }
  });
});

describe("resolveNetemRequest", () => {
  it("resolves a named profile to its params", () => {
    const action = resolveNetemRequest({ profile: "lossy_mobile" });
    expect(action).toEqual({
      op: "shape",
      label: "lossy_mobile",
      params: NETEM_PROFILES.lossy_mobile,
    });
  });

  it('treats "clean" and "none" as clear', () => {
    expect(resolveNetemRequest({ profile: "clean" })).toEqual({ op: "clear", label: "clean" });
    expect(resolveNetemRequest({ profile: "none" })).toEqual({ op: "clear", label: "none" });
  });

  it("honors an explicit { clear: true }", () => {
    expect(resolveNetemRequest({ clear: true })).toEqual({ op: "clear", label: "clear" });
  });

  it("resolves raw params to a custom shape action", () => {
    expect(resolveNetemRequest({ delayMs: 200, lossPct: 3 })).toEqual({
      op: "shape",
      label: "custom",
      params: { delayMs: 200, lossPct: 3 },
    });
  });

  it("rejects an unknown profile", () => {
    expect(() => resolveNetemRequest({ profile: "turbo" })).toThrow(/unknown profile/);
  });

  it.each(["toString", "constructor"])("rejects the inherited Object key %j", (name) => {
    expect(() => resolveNetemRequest({ profile: name })).toThrow(/unknown profile/);
  });

  it("rejects profile + raw params together (ambiguous)", () => {
    expect(() => resolveNetemRequest({ profile: "satellite", delayMs: 10 })).toThrow(/not both/);
  });

  it.each(NETEM_PARAM_KEYS)("rejects %s alongside a profile or a clear", (key) => {
    expect(() => resolveNetemRequest({ profile: "satellite", [key]: 10 })).toThrow(/not both/);
    expect(() => resolveNetemRequest({ clear: true, [key]: 10 })).toThrow(/cannot be combined/);
  });

  it("rejects a non-object body", () => {
    expect(() => resolveNetemRequest(null)).toThrow(NetemValidationError);
    expect(() => resolveNetemRequest([])).toThrow(NetemValidationError);
    expect(() => resolveNetemRequest("x")).toThrow(NetemValidationError);
  });

  it("rejects an empty body", () => {
    expect(() => resolveNetemRequest({})).toThrow(/empty request/);
  });

  it("NEVER reads an interface from the request body (injection guard)", () => {
    // A body cannot set the shaped interface — iface is server/deploy
    // config only. An `iface` field is ignored, so a malicious value
    // can never reach the argv.
    const action = resolveNetemRequest({ profile: "satellite", iface: "eth0; rm -rf /" });
    expect(action).toEqual({
      op: "shape",
      label: "satellite",
      params: NETEM_PROFILES.satellite,
    });
    expect(JSON.stringify(action)).not.toContain("rm -rf");
  });
});

describe("applyNetemAction", () => {
  const congested = NETEM_PROFILES.congested_wifi!;
  const egressOnly = { delayMs: 80, lossPct: 2, rateKbit: 2_000 };

  it("installs a profile's ingress mirror, every ip through netem-setpriv and tc bare", async () => {
    // A fresh pod: no ifb0 and no ingress hook to delete.
    const { exec, calls } = podExec(BOTH_SHAPED, [
      { needle: "link show", msg: 'Device "ifb0" does not exist.' },
      {
        needle: "qdisc del dev eth0 ingress",
        msg: "RTNETLINK answers: No such file or directory",
        status: 2,
      },
    ]);
    const result = await applyNetemAction(
      { op: "shape", label: "congested_wifi", params: congested },
      { iface: "eth0", exec },
    );
    const ifbArgs = buildNetemShapeArgs(NETEM_IFB_DEV, ingressNetemParams(congested));
    expect(calls).toEqual([
      ["tc", ...buildNetemShapeArgs("eth0", congested)],
      [...SETPRIV_IP, "link", "show", "ifb0"],
      [...SETPRIV_IP, "link", "add", "ifb0", "type", "ifb"],
      [...SETPRIV_IP, "link", "set", "ifb0", "up"],
      [...SETPRIV_IP, "link", "set", "ifb0", "txqueuelen", "1000"],
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
      ["tc", "qdisc", "add", "dev", "eth0", "handle", "ffff:", "ingress"],
      [
        ...["tc", "filter", "add", "dev", "eth0", "parent", "ffff:", "protocol", "all"],
        ...["u32", "match", "u32", "0", "0", "action", "mirred", "egress", "redirect"],
        ...["dev", "ifb0"],
      ],
      ["tc", ...ifbArgs],
      ["tc", "qdisc", "show", "dev", "eth0"],
      ["tc", "qdisc", "show", "dev", "ifb0"],
    ]);
    expect(ifbArgs).toContain(`${congested.downlinkRateKbit}kbit`);
    expect(result).toEqual({
      commands: calls,
      label: "congested_wifi",
      op: "shape",
      ingressShaped: true,
      mirrorRemoved: false,
      readback: { eth0: BOTH_SHAPED.eth0, ifb0: IFB_NETEM },
    });
  });

  it("execs ip through the injected setpriv path", async () => {
    const { exec, calls } = podExec(BOTH_SHAPED);
    await applyNetemAction(
      { op: "shape", label: "congested_wifi", params: congested },
      { iface: "eth0", exec, setpriv: "/opt/caps/setpriv" },
    );
    const ip = calls.filter((c) => c.includes("ip"));
    expect(ip.length).toBeGreaterThan(0);
    for (const c of ip)
      expect(c.slice(0, 7)).toEqual(["/opt/caps/setpriv", ...SETPRIV_IP.slice(1)]);
    expect(calls.filter((c) => c[0] === "ip")).toEqual([]);
  });

  it("installs past a stale-hook delete that finds no hook", async () => {
    const { exec } = podExec(BOTH_SHAPED, {
      needle: "qdisc del dev eth0 ingress",
      msg: "Error: Invalid handle.",
    });
    await expect(
      applyNetemAction(
        { op: "shape", label: "congested_wifi", params: congested },
        { iface: "eth0", exec },
      ),
    ).resolves.toMatchObject({ ingressShaped: true });
  });

  afterEach(() => {
    vi.unstubAllEnvs();
  });

  it("defaults the setpriv path to NETEM_SETPRIV from the environment", async () => {
    vi.stubEnv("NETEM_SETPRIV", "/opt/env/setpriv");
    const { exec, calls } = podExec({});
    await applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec });
    expect(calls).toContainEqual([
      "/opt/env/setpriv",
      ...SETPRIV_IP.slice(1),
      "link",
      "del",
      "ifb0",
    ]);
  });

  it("reuses an ifb0 that already exists instead of adding a second", async () => {
    const { exec, calls } = podExec(BOTH_SHAPED);
    await applyNetemAction(
      { op: "shape", label: "congested_wifi", params: congested },
      { iface: "eth0", exec },
    );
    expect(calls.some((c) => c.includes("add") && c.includes("ifb"))).toBe(false);
  });

  it("fails a mirror install step that exits non-zero, saying egress is already shaped", async () => {
    const { exec } = podExec(BOTH_SHAPED, { needle: "ffff: ingress", msg: "Exclusivity flag on" });
    const err = await applyNetemAction(
      { op: "shape", label: "congested_wifi", params: congested },
      { iface: "eth0", exec },
    ).then(
      () => null,
      (e: unknown) => e as NetemStateError,
    );
    expect(err).toBeInstanceOf(NetemStateError);
    expect(err!.message).toMatch(/shaped egress on eth0 but .* failed to install: Exclusivity/);
    expect(err!.result).toMatchObject({
      ingressShaped: false,
      mirrorRemoved: true,
      readback: { eth0: BOTH_SHAPED.eth0, ifb0: IFB_NETEM },
    });
  });

  it("reports no mirror removed when the redirect is back and only ifb0's netem fails", async () => {
    const { exec } = podExec(BOTH_SHAPED, { needle: "replace dev ifb0", msg: "RTNETLINK answers" });
    await expect(
      applyNetemAction(
        { op: "shape", label: "congested_wifi", params: congested },
        { iface: "eth0", exec },
      ),
    ).rejects.toMatchObject({ name: "NetemStateError", result: { mirrorRemoved: false } });
  });

  it("reports no mirror removed when an install fails on a pod that had none", async () => {
    const { exec } = podExec({ eth0: ETH_NETEM }, [
      { needle: "del dev eth0 ingress", msg: "Cannot find specified qdisc" },
      { needle: "ffff: ingress", msg: "Exclusivity flag on" },
    ]);
    await expect(
      applyNetemAction(
        { op: "shape", label: "congested_wifi", params: congested },
        { iface: "eth0", exec },
      ),
    ).rejects.toMatchObject({ name: "NetemStateError", result: { mirrorRemoved: false } });
  });

  it("shapes raw params with the downlink pair in both directions", async () => {
    const { exec, calls } = podExec(BOTH_SHAPED);
    const params = { ...egressOnly, downlinkRateKbit: 4_000, ingressLimitPkts: 55 };
    const result = await applyNetemAction(
      { op: "shape", label: "custom", params },
      { iface: "eth0", exec },
    );
    expect(result.ingressShaped).toBe(true);
    expect(calls).toContainEqual([
      "tc",
      ...buildNetemShapeArgs(NETEM_IFB_DEV, ingressNetemParams(params)),
    ]);
  });

  it("shapes raw params without the downlink pair on egress only, and says so", async () => {
    const { exec, calls } = podExec({ eth0: ETH_NETEM });
    const result = await applyNetemAction(
      { op: "shape", label: "custom", params: egressOnly },
      { iface: "eth0", exec },
    );
    expect(result.ingressShaped).toBe(false);
    expect(calls.some((c) => c.includes("add"))).toBe(false);
    expect(calls.some((c) => c.join(" ").includes(`show dev ${NETEM_IFB_DEV}`))).toBe(false);
    expect(result.readback).toEqual({ eth0: ETH_NETEM });
  });

  it("runs the exact clear sequence, ip link del through netem-setpriv", async () => {
    const { exec, calls } = podExec({});
    const result = await applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec });
    expect(calls).toEqual([
      ["tc", "qdisc", "del", "dev", "eth0", "root"],
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
      ["tc", "qdisc", "del", "dev", "ifb0", "root"],
      [...SETPRIV_IP, "link", "del", "ifb0"],
      ["tc", "qdisc", "show", "dev", "eth0"],
    ]);
    expect(result).toMatchObject({ op: "clear", ingressShaped: false, mirrorRemoved: true });
  });

  it.each(["shape", "clear"] as const)(
    "tears the ingress mirror down on an egress-only %s",
    async (op) => {
      const { exec } = podExec({ eth0: op === "shape" ? ETH_NETEM : "" });
      const action =
        op === "shape"
          ? ({ op, label: "custom", params: egressOnly } as const)
          : ({ op, label: "clear" } as const);
      const result = await applyNetemAction(action, { iface: "eth0", exec });
      expect(result.mirrorRemoved).toBe(true);
      expect(result.commands.slice(1, -1)).toEqual(ran(buildNetemMirrorClearArgs("eth0")));
    },
  );

  it("fails a clear whose ip link del exits non-zero, reporting ifb0 NOT removed", async () => {
    const { exec } = podExec({}, { needle: "link del", msg: "Operation not permitted" });
    const err = await applyNetemAction(
      { op: "clear", label: "clear" },
      { iface: "eth0", exec },
    ).then(
      () => null,
      (e: unknown) => e as NetemStateError,
    );
    expect(err).toBeInstanceOf(NetemStateError);
    expect(err!.message).toMatch(/ifb0 was NOT removed: Operation not permitted/);
    expect(err!.result.mirrorRemoved).toBe(false);
    expect(err!.result.commands).toContainEqual([...SETPRIV_IP, "link", "del", "ifb0"]);
    expect(err!.result.readback).toEqual({ eth0: "", ifb0: "" });
  });

  it.each(['Cannot find device "ifb0"', 'Device "ifb0" does not exist.'])(
    "treats an ip link del of an already-absent ifb0 (%s) as removed",
    async (msg) => {
      const { exec } = podExec({}, { needle: "link del", msg });
      await expect(
        applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
      ).resolves.toMatchObject({ op: "clear", mirrorRemoved: true });
    },
  );

  it.each(["RTNETLINK answers: Operation not permitted", "Device or resource busy"])(
    "still fails an ip link del that exits non-zero with %s",
    async (msg) => {
      const { exec } = podExec({}, { needle: "link del", msg });
      await expect(
        applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
      ).rejects.toMatchObject({ name: "NetemStateError", result: { mirrorRemoved: false } });
    },
  );

  it("fails an absent-device wording from an ip link del that never ran", async () => {
    const { exec } = podExec(
      {},
      { needle: "link del", msg: 'Cannot find device "ifb0"', status: 127 },
    );
    await expect(
      applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
    ).rejects.toThrow(NetemStateError);
  });

  it("leaves an ifb device alone when this interface carries no mirror hook", async () => {
    // The hook delete's exit status is the only proof the mirror was ours.
    const { exec, calls } = failingOnlyExec("qdisc del dev eth0 ingress", "Error: Invalid handle.");
    const result = await applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec });
    expect(result.mirrorRemoved).toBe(false);
    expect(calls.map((c) => [c.file, ...c.args])).toEqual([
      ["tc", ...buildNetemClearArgs("eth0")],
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
      ["tc", ...buildNetemProbeArgs("eth0")],
    ]);
    expect(calls.some((c) => c.args.includes(NETEM_IFB_DEV))).toBe(false);
  });

  it("removes the hook BEFORE the device it redirects onto", async () => {
    const { exec } = recordingExec();
    const { commands } = await applyNetemAction(
      { op: "clear", label: "clear" },
      { iface: "eth0", exec },
    );
    const hook = commands.findIndex((c) => c.includes("ingress"));
    const del = commands.findIndex((c) => c.includes("ip") && c.includes("del"));
    expect(hook).toBeGreaterThanOrEqual(0);
    expect(hook).toBeLessThan(del);
  });

  it.each([
    ["ifb0", { eth0: `${ETH_NETEM}\n${ETH_HOOK}`, ifb0: "qdisc noqueue 0: root refcnt 2" }],
    ["eth0", { eth0: `qdisc noqueue 0: root refcnt 2\n${ETH_HOOK}`, ifb0: IFB_NETEM }],
  ])("fails a profile shape whose post-read finds no netem on %s", async (missing, show) => {
    const { exec } = podExec(show);
    const err = await applyNetemAction(
      { op: "shape", label: "congested_wifi", params: congested },
      { iface: "eth0", exec },
    ).then(
      () => null,
      (e: unknown) => e as NetemStateError,
    );
    expect(err).toBeInstanceOf(NetemStateError);
    expect(err!.message).toContain(`no netem on ${missing}`);
    expect(err!.result.readback).toEqual({ eth0: show.eth0, ifb0: show.ifb0 });
    expect(err!.result.ingressShaped).toBe(false);
  });

  it("fails a profile shape whose post-read cannot read ifb0", async () => {
    const { exec } = podExec(BOTH_SHAPED, {
      needle: "show dev ifb0",
      msg: 'Cannot find device "ifb0"',
    });
    await expect(
      applyNetemAction(
        { op: "shape", label: "congested_wifi", params: congested },
        { iface: "eth0", exec },
      ),
    ).rejects.toThrow(/no netem on ifb0; read back eth0=\[.*\] ifb0=\[unread: Cannot find device/s);
  });

  it("fails a profile shape whose post-read finds no ingress hook on the interface", async () => {
    const { exec } = podExec({ eth0: ETH_NETEM, ifb0: IFB_NETEM });
    await expect(
      applyNetemAction(
        { op: "shape", label: "congested_wifi", params: congested },
        { iface: "eth0", exec },
      ),
    ).rejects.toThrow(/no ingress hook on eth0/);
  });

  it.each(["shape", "clear"] as const)(
    "refuses to report an egress-only %s that left the mirror hook installed",
    async (op) => {
      // Every command "succeeds" yet the post-read still shows the hook: the
      // exact shape of a step that reported success and did nothing.
      const { exec } = podExec({ eth0: `${op === "shape" ? ETH_NETEM : ""}\n${ETH_HOOK}` });
      const action =
        op === "shape"
          ? ({ op, label: "custom", params: egressOnly } as const)
          : ({ op, label: "clear" } as const);
      await expect(applyNetemAction(action, { iface: "eth0", exec })).rejects.toThrow(
        /an ingress mirror left on eth0/,
      );
    },
  );

  it("refuses to report a clear that left a netem qdisc installed", async () => {
    const { exec } = podExec({ eth0: ETH_NETEM });
    await expect(
      applyNetemAction({ op: "clear", label: "clean" }, { iface: "eth0", exec }),
    ).rejects.toThrow(/a netem qdisc left on eth0/);
  });

  it("fails an egress shape whose post-read finds no netem", async () => {
    const { exec } = podExec({ eth0: "qdisc noqueue 0: root refcnt 2" });
    await expect(
      applyNetemAction(
        { op: "shape", label: "custom", params: egressOnly },
        { iface: "eth0", exec },
      ),
    ).rejects.toThrow(/no netem on eth0/);
  });

  it("fails when the post-read itself cannot run, rather than reporting the action", async () => {
    const { exec } = failingOnlyExec("qdisc show", "tc: command not found", 127);
    await expect(
      applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
    ).rejects.toThrow(/command not found/);
  });

  it.each([126, 127])(
    "fails a mirror teardown step that never executed (rc=%i)",
    async (exitStatus) => {
      const { exec } = failingOnlyExec(
        "qdisc del dev eth0 ingress",
        "/usr/sbin/tc: bad interpreter: No such file or directory",
        exitStatus,
      );
      await expect(
        applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
      ).rejects.toThrow(/bad interpreter/);
    },
  );

  it.each([1, 2, 125])(
    "swallows a benign 'No such file' clear that tc itself exited %s with",
    async (exitStatus) => {
      const { exec } = failingOnlyExec(
        "qdisc del dev eth0 root",
        "RTNETLINK answers: No such file or directory",
        exitStatus,
      );
      await expect(
        applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
      ).resolves.toMatchObject({ op: "clear" });
    },
  );

  it.each([126, 127, 128, null])(
    "fails a benign-wording clear at status %s, which tc did not produce",
    async (exitStatus) => {
      const exec = failingExec("RTNETLINK answers: No such file or directory", exitStatus);
      await expect(
        applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
      ).rejects.toThrow(/No such file/);
    },
  );

  it("fails a benign-wording clear rejected with a plain Error, not a NetemExecError", async () => {
    const exec: NetemExec = async () => {
      throw new Error("RTNETLINK answers: No such file or directory");
    };
    await expect(
      applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
    ).rejects.toThrow(/No such file/);
  });

  it("rethrows a real failure (e.g. missing NET_ADMIN) when clearing", async () => {
    const exec = failingExec("Operation not permitted");
    await expect(
      applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
    ).rejects.toThrow(/not permitted/);
  });

  it("rethrows a shape failure", async () => {
    const exec = failingExec("Operation not permitted");
    await expect(
      applyNetemAction(
        { op: "shape", label: "custom", params: { lossPct: 1 } },
        { iface: "eth0", exec },
      ),
    ).rejects.toThrow(/not permitted/);
  });
});

describe("netemSetprivPath", () => {
  it("resolves like the entrypoint's ${NETEM_SETPRIV:-default}", () => {
    expect(netemSetprivPath({})).toBe(NETEM_SETPRIV_DEFAULT);
    expect(netemSetprivPath({ NETEM_SETPRIV: "" })).toBe(NETEM_SETPRIV_DEFAULT);
    expect(netemSetprivPath({ NETEM_SETPRIV: "/opt/caps/setpriv" })).toBe("/opt/caps/setpriv");
  });
});

describe("defaultNetemExec", () => {
  it("reports the child's own exit status when it ran and exited non-zero", async () => {
    const exec = defaultNetemExec();
    await expect(exec(process.execPath, ["-e", "process.exit(2)"])).rejects.toMatchObject({
      name: "NetemExecError",
      exitStatus: 2,
    });
  });

  it("reports no exit status when the binary could not be spawned", async () => {
    const exec = defaultNetemExec();
    const err = await exec("videocall-bots-no-such-binary", ["qdisc", "show"]).then(
      () => null,
      (e: unknown) => e as NetemExecError,
    );
    expect(err?.name).toBe("NetemExecError");
    expect(err?.exitStatus).toBeNull();
    expect(netemExecExitedNonZero(err)).toBe(false);
  });
});

describe("the ingress mirror's argv (#2353)", () => {
  const SHAPING = Object.entries(NETEM_PROFILES).filter(([, p]) => p !== null) as Array<
    [string, NonNullable<(typeof NETEM_PROFILES)[string]>]
  >;

  it.each(SHAPING)("shapes %s's ingress at its downlink rate, not its uplink", (name, params) => {
    const ingress = ingressNetemParams(params);
    expect(ingress.rateKbit, `${name} must shape ingress at its downlink rate`).toBe(
      params.downlinkRateKbit,
    );
    // Everything else symmetric: the profiles model delay/jitter/loss one way.
    expect({ ...ingress, rateKbit: params.rateKbit, limitPkts: params.limitPkts }).toEqual({
      ...params,
      downlinkRateKbit: undefined,
      ingressLimitPkts: undefined,
    });
  });

  it("shapes ingress at its own queue depth, not the egress one", () => {
    for (const [name, params] of SHAPING) {
      expect(ingressNetemParams(params).limitPkts, name).toBe(params.ingressLimitPkts);
    }
    expect(ingressNetemParams(NETEM_PROFILES.satellite!).limitPkts).not.toBe(
      NETEM_PROFILES.satellite!.limitPkts,
    );
  });

  it("refuses to build a mirror for params carrying no downlink rate or depth", () => {
    expect(() => ingressNetemParams({ lossPct: 1, rateKbit: 800 })).toThrow(NetemValidationError);
    expect(() =>
      ingressNetemParams({ ...NETEM_PROFILES.satellite!, ingressLimitPkts: undefined }),
    ).toThrow(/ingressLimitPkts/);
    expect(() => buildNetemMirrorInstallArgs("eth0", { lossPct: 1 })).toThrow(/downlinkRateKbit/);
  });

  it("never leaks the downlink rate into the egress argv", () => {
    for (const [name, params] of SHAPING) {
      const args = buildNetemShapeArgs("eth0", params);
      expect(args, name).toContain(`${params.rateKbit}kbit`);
      expect(
        args.filter((a) => a.endsWith("kbit")),
        name,
      ).toEqual([`${params.rateKbit}kbit`]);
    }
  });

  it("installs the mirror in the order real iproute2 requires", () => {
    const cmds = buildNetemMirrorInstallArgs("eth0", NETEM_PROFILES.congested_wifi!);
    expect(flat(cmds.filter((c) => c.tolerateFailure))).toEqual([
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
    ]);
    expect(flat(cmds)).toEqual([
      ["ip", "link", "show", NETEM_IFB_DEV],
      ["ip", "link", "add", NETEM_IFB_DEV, "type", "ifb"],
      ["ip", "link", "set", NETEM_IFB_DEV, "up"],
      ["ip", "link", "set", NETEM_IFB_DEV, "txqueuelen", `${NETEM_IFB_TXQUEUELEN}`],
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
      ["tc", "qdisc", "add", "dev", "eth0", "handle", "ffff:", "ingress"],
      [
        ...["tc", "filter", "add", "dev", "eth0", "parent", "ffff:", "protocol", "all"],
        ...["u32", "match", "u32", "0", "0"],
        ...["action", "mirred", "egress", "redirect", "dev", NETEM_IFB_DEV],
      ],
      [
        "tc",
        ...buildNetemShapeArgs(NETEM_IFB_DEV, ingressNetemParams(NETEM_PROFILES.congested_wifi!)),
      ],
    ]);
    // The step the caller must skip when the device already exists.
    expect(flat(cmds)[NETEM_MIRROR_ADD_STEP]).toEqual([
      "ip",
      "link",
      "add",
      NETEM_IFB_DEV,
      "type",
      "ifb",
    ]);
    // `protocol ip` would leave an IPv6-resolved relay unshaped.
    const filter = cmds.find((c) => c.args[0] === "filter")!;
    expect(filter.args).toContain("all");
    expect(filter.args).not.toContain("ip");
  });

  it("tears the mirror down hook-first, then the device", () => {
    expect(flat(buildNetemMirrorClearArgs("eth0"))).toEqual([
      ["tc", "qdisc", "del", "dev", "eth0", "ingress"],
      ["tc", "qdisc", "del", "dev", NETEM_IFB_DEV, "root"],
      ["ip", "link", "del", NETEM_IFB_DEV],
    ]);
  });

  it.each([buildNetemMirrorClearArgs, buildNetemProbeArgs])(
    "validates the interface name before it reaches argv",
    (build) => {
      expect(() => build("eth0; rm -rf /")).toThrow(NetemValidationError);
      for (const iface of LOOPBACK_IFACES) expect(() => build(iface)).toThrow(NetemValidationError);
    },
  );

  it("accepts a request-supplied downlink pair, validated like its egress twin", () => {
    expect(
      validateNetemParams({ lossPct: 1, downlinkRateKbit: 4000, ingressLimitPkts: 55 }),
    ).toEqual({ lossPct: 1, downlinkRateKbit: 4000, ingressLimitPkts: 55 });
    expect(() => validateNetemParams({ lossPct: 1, downlinkRateKbit: 4000 })).toThrow(
      /supply both or neither/,
    );
    expect(() => validateNetemParams({ lossPct: 1, ingressLimitPkts: 55 })).toThrow(
      /supply both or neither/,
    );
    expect(() =>
      validateNetemParams({ lossPct: 1, downlinkRateKbit: 4, ingressLimitPkts: 55 }),
    ).toThrow(/"downlinkRateKbit" must be >= 8/);
    expect(() =>
      validateNetemParams({ lossPct: 1, downlinkRateKbit: 4000, ingressLimitPkts: 1.5 }),
    ).toThrow(/"ingressLimitPkts" must be an integer >= 1/);
    expect(() => resolveNetemRequest({ downlinkRateKbit: 4000, ingressLimitPkts: 55 })).toThrow(
      /at least one of/,
    );
  });
});
