import { describe, expect, it } from "vitest";

import {
  applyNetemAction,
  buildNetemClearArgs,
  buildNetemShapeArgs,
  NETEM_PROFILES,
  type NetemExec,
  NetemValidationError,
  resolveNetemRequest,
  validateNetemParams,
} from "./netem";

/** A recording exec: captures every `tc` invocation, never runs one. */
function recordingExec(): {
  exec: NetemExec;
  calls: Array<{ file: string; args: string[] }>;
} {
  const calls: Array<{ file: string; args: string[] }> = [];
  const exec: NetemExec = async (file, args) => {
    calls.push({ file, args });
    return { stdout: "", stderr: "" };
  };
  return { exec, calls };
}

/** An exec that always rejects with `msg` — models a `tc` failure. */
function failingExec(msg: string): NetemExec {
  return async () => {
    throw new Error(msg);
  };
}

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
});

describe("buildNetemClearArgs", () => {
  it("builds `qdisc del dev <iface> root`", () => {
    expect(buildNetemClearArgs("eth0")).toEqual(["qdisc", "del", "dev", "eth0", "root"]);
  });

  it("rejects an unsafe interface name", () => {
    expect(() => buildNetemClearArgs("eth0; echo hi")).toThrow(NetemValidationError);
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
  it("runs the exact `tc` shape command via the injected exec", async () => {
    const { exec, calls } = recordingExec();
    const result = await applyNetemAction(
      { op: "shape", label: "lossy_mobile", params: NETEM_PROFILES.lossy_mobile! },
      { iface: "eth0", exec },
    );
    expect(calls).toHaveLength(1);
    expect(calls[0].file).toBe("tc");
    expect(calls[0].args).toEqual(buildNetemShapeArgs("eth0", NETEM_PROFILES.lossy_mobile!));
    expect(result.argv[0]).toBe("tc");
    expect(result.op).toBe("shape");
    expect(result.label).toBe("lossy_mobile");
  });

  it("runs the exact `tc` clear command via the injected exec", async () => {
    const { exec, calls } = recordingExec();
    const result = await applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec });
    expect(calls).toHaveLength(1);
    expect(calls[0].args).toEqual(["qdisc", "del", "dev", "eth0", "root"]);
    expect(result.op).toBe("clear");
  });

  it("swallows a benign 'No such file' error when clearing an already-clean interface", async () => {
    const exec = failingExec("RTNETLINK answers: No such file or directory");
    await expect(
      applyNetemAction({ op: "clear", label: "clear" }, { iface: "eth0", exec }),
    ).resolves.toMatchObject({ op: "clear" });
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
