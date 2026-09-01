import { describe, expect, it } from "vitest";

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
  NETEM_PROFILES,
  type NetemCommand,
  type NetemExec,
  NetemExecError,
  netemExecExitedNonZero,
  NetemStateError,
  NetemValidationError,
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

  it("caps impairment BELOW total so the control channel stays reachable (self-DoS guard)", () => {
    // netem shapes the pod's OWN eth0 egress, which also carries the control
    // server's responses (incl. the DELETE /netem that clears it). Total
    // impairment would strand the API — recoverable only by out-of-band
    // `kubectl exec … tc qdisc del`. These bounds keep the channel usable.
    // lossPct is capped at 95, not 100:
    expect(validateNetemParams({ lossPct: 95 }).lossPct).toBe(95);
    expect(() => validateNetemParams({ lossPct: 96 })).toThrow(/<= 95/);
    expect(() => validateNetemParams({ lossPct: 100 })).toThrow(/<= 95/);
    // rateKbit is floored at 8:
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

  it("accepts every shipped profile's runtime-applicable params unchanged", () => {
    for (const [name, params] of Object.entries(NETEM_PROFILES)) {
      if (params === null) continue;
      const { downlinkRateKbit, ...runtime } = params;
      expect(downlinkRateKbit, `${name} must carry a downlink rate`).toBeGreaterThan(0);
      expect(validateNetemParams({ ...runtime }), name).toEqual(runtime);
      // The ingress half is startup-only, so posting it verbatim is refused.
      expect(() => validateNetemParams({ ...params }), name).toThrow(NetemValidationError);
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
  /** Every command a runtime action runs, first the op then the teardown. */
  const expected = (first: string[]): string[][] => [
    first,
    ...flat(buildNetemMirrorClearArgs("eth0")),
    ["tc", ...buildNetemProbeArgs("eth0")],
  ];

  it("runs the exact `tc` shape command via the injected exec", async () => {
    const { exec, calls } = recordingExec();
    const result = await applyNetemAction(
      { op: "shape", label: "lossy_mobile", params: NETEM_PROFILES.lossy_mobile! },
      { iface: "eth0", exec },
    );
    expect(calls[0].file).toBe("tc");
    expect(calls[0].args).toEqual(buildNetemShapeArgs("eth0", NETEM_PROFILES.lossy_mobile!));
    expect(result.commands).toEqual(
      expected(["tc", ...buildNetemShapeArgs("eth0", NETEM_PROFILES.lossy_mobile!)]),
    );
    expect(result.op).toBe("shape");
    expect(result.label).toBe("lossy_mobile");
  });

  it("runs the exact `tc` clear command via the injected exec", async () => {
    const { exec, calls } = recordingExec();
    const result = await applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec });
    expect(calls[0].args).toEqual(["qdisc", "del", "dev", "eth0", "root"]);
    expect(result.commands).toEqual(expected(["tc", "qdisc", "del", "dev", "eth0", "root"]));
    expect(result.op).toBe("clear");
  });

  // Startup shapes ingress; a runtime action cannot, so it must not report its
  // own label over a downlink still shaped by the profile startup applied.
  it.each(["shape", "clear"] as const)("tears the ingress mirror down on a %s", async (op) => {
    const { exec, calls } = recordingExec();
    const action =
      op === "shape"
        ? ({ op, label: "good_4g", params: NETEM_PROFILES.good_4g! } as const)
        : ({ op, label: "clear" } as const);
    const result = await applyNetemAction(action, { iface: "eth0", exec });
    expect(result.mirrorRemoved).toBe(true);
    expect(calls.map((c) => [c.file, ...c.args])).toEqual(
      result.commands.slice(0, result.commands.length),
    );
    expect(result.commands.slice(1, -1)).toEqual(flat(buildNetemMirrorClearArgs("eth0")));
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
    const del = commands.findIndex((c) => c[0] === "ip" && c.includes("del"));
    expect(hook).toBeGreaterThanOrEqual(0);
    expect(hook).toBeLessThan(del);
  });

  it.each(["shape", "clear"] as const)(
    "refuses to report a %s that left the mirror hook installed",
    async (op) => {
      // Every command "succeeds" yet the post-read still shows the hook: the
      // exact shape of a step that reported success and did nothing.
      const exec: NetemExec = async () => ({
        stdout: `qdisc noqueue 0: root refcnt 2\n${NETEM_INGRESS_QDISC_MARKER} ffff: parent ffff:fff1`,
        stderr: "",
      });
      const action =
        op === "shape"
          ? ({ op, label: "good_4g", params: NETEM_PROFILES.good_4g! } as const)
          : ({ op, label: "clear" } as const);
      await expect(applyNetemAction(action, { iface: "eth0", exec })).rejects.toThrow(
        NetemStateError,
      );
    },
  );

  it("refuses to report a clear that left a netem qdisc installed", async () => {
    const exec: NetemExec = async () => ({
      stdout: "qdisc netem 8001: root refcnt 2 limit 55 delay 80ms 30ms loss 2% rate 2Mbit",
      stderr: "",
    });
    await expect(
      applyNetemAction({ op: "clear", label: "clean" }, { iface: "eth0", exec }),
    ).rejects.toThrow(/left a netem qdisc/);
  });

  it("still reports a shape whose own netem the post-read finds", async () => {
    const exec: NetemExec = async () => ({
      stdout: "qdisc netem 8001: root refcnt 2 limit 100 delay 50ms 15ms loss 0.5% rate 10Mbit",
      stderr: "",
    });
    await expect(
      applyNetemAction(
        { op: "shape", label: "good_4g", params: NETEM_PROFILES.good_4g! },
        { iface: "eth0", exec },
      ),
    ).resolves.toMatchObject({ op: "shape" });
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
    expect({ ...ingress, rateKbit: params.rateKbit }).toEqual({
      ...params,
      downlinkRateKbit: undefined,
      rateKbit: params.rateKbit,
    });
  });

  it("keeps the ingress queue depth equal to the egress depth, deliberately", () => {
    for (const [name, params] of SHAPING) {
      expect(ingressNetemParams(params).limitPkts, name).toBe(params.limitPkts);
    }
  });

  it("refuses to build a mirror for params carrying no downlink rate", () => {
    expect(() => ingressNetemParams({ lossPct: 1, rateKbit: 800 })).toThrow(NetemValidationError);
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

  it("rejects a request-supplied downlink rate, which nothing would apply", () => {
    expect(() => validateNetemParams({ lossPct: 1, downlinkRateKbit: 4000 })).toThrow(
      /not accepted at runtime/,
    );
    expect(() => resolveNetemRequest({ downlinkRateKbit: 4000 })).toThrow(NetemValidationError);
  });
});
