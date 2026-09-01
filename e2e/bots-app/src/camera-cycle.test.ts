import { describe, expect, it, vi } from "vitest";

import {
  CAMERA_CYCLE_APPLIED_BANNER,
  CAMERA_CYCLE_DEGRADED_BANNER,
  CAMERA_CYCLE_ENV,
  CAMERA_CYCLE_NEVER_FIRED_BANNER,
  CAMERA_CYCLE_SECS_CEILING,
  type CameraCycleConfig,
  type CameraCycleLocator,
  type CameraCyclePage,
  formatCameraCycleConfig,
  formatCameraCycleReceipt,
  newCameraCycleTally,
  nextPhaseMs,
  recordCameraPhase,
  resolveCameraCycle,
  setCameraEnabled,
  startCameraCycle,
  targetDutyPct,
} from "./camera-cycle";
import { ACTION_BAR_SELECTOR, cameraButtonSelector } from "./control-buttons";

const FULL = { onMin: "5", onMax: "15", offMin: "20", offMax: "60" };

describe("resolveCameraCycle", () => {
  it("returns undefined when all four are unset — the camera-always-on default", () => {
    for (const raw of [{}, { onMin: "", onMax: "  ", offMin: undefined, offMax: "" }]) {
      const r = resolveCameraCycle(raw);
      expect(r).toEqual({ kind: "ok", value: undefined });
    }
  });

  it("resolves seconds to ms", () => {
    expect(resolveCameraCycle(FULL)).toEqual({
      kind: "ok",
      value: { onMinMs: 5_000, onMaxMs: 15_000, offMinMs: 20_000, offMaxMs: 60_000 },
    });
  });

  it("accepts min == max (a fixed-length phase)", () => {
    const r = resolveCameraCycle({ onMin: "10", onMax: "10", offMin: "30", offMax: "30" });
    expect(r.kind).toBe("ok");
  });

  // The failure the issue's "unset ⇒ unchanged" default makes dangerous: three
  // set + one missing must NOT collapse to always-on.
  it.each([
    ["onMax", CAMERA_CYCLE_ENV.onMax],
    ["offMin", CAMERA_CYCLE_ENV.offMin],
    ["offMax", CAMERA_CYCLE_ENV.offMax],
    ["onMin", CAMERA_CYCLE_ENV.onMin],
  ] as const)(
    "rejects a partial set rather than silently disabling (%s missing)",
    (field, name) => {
      const r = resolveCameraCycle({ ...FULL, [field]: "" });
      expect(r.kind).toBe("invalid");
      if (r.kind !== "invalid") throw new Error("unreachable");
      expect(r.message).toContain("all four");
      expect(r.message).toContain(name);
    },
  );

  it.each([
    ["abc", "at most 5 digits"],
    ["-1", "at most 5 digits"],
    ["1.5", "at most 5 digits"],
    ["10s", "at most 5 digits"],
    ["000000", "at most 5 digits"],
    ["0", ">= 1 second"],
    ["86401", `<= ${CAMERA_CYCLE_SECS_CEILING}`],
  ])("rejects onMin=%j", (value, reason) => {
    const r = resolveCameraCycle({ ...FULL, onMin: value });
    expect(r.kind).toBe("invalid");
    if (r.kind !== "invalid") throw new Error("unreachable");
    expect(r.message).toContain(CAMERA_CYCLE_ENV.onMin);
    expect(r.message).toContain(reason);
  });

  it("rejects MIN > MAX on both phases", () => {
    const on = resolveCameraCycle({ ...FULL, onMin: "30" });
    expect(on.kind).toBe("invalid");
    if (on.kind === "invalid") expect(on.message).toContain(CAMERA_CYCLE_ENV.onMax);
    const off = resolveCameraCycle({ ...FULL, offMin: "999" });
    expect(off.kind).toBe("invalid");
    if (off.kind === "invalid") expect(off.message).toContain(CAMERA_CYCLE_ENV.offMax);
  });

  it("accepts the ceiling itself and a leading zero as base 10", () => {
    const r = resolveCameraCycle({
      onMin: "08",
      onMax: String(CAMERA_CYCLE_SECS_CEILING),
      offMin: "09",
      offMax: "10",
    });
    expect(r).toEqual({
      kind: "ok",
      value: {
        onMinMs: 8_000,
        onMaxMs: CAMERA_CYCLE_SECS_CEILING * 1_000,
        offMinMs: 9_000,
        offMaxMs: 10_000,
      },
    });
  });
});

const CFG: CameraCycleConfig = {
  onMinMs: 5_000,
  onMaxMs: 15_000,
  offMinMs: 20_000,
  offMaxMs: 60_000,
};

describe("targetDutyPct / formatCameraCycleConfig", () => {
  it("is the mean-on share of the mean cycle, truncated", () => {
    // (5+15) / (5+15+20+60) = 20%
    expect(targetDutyPct(CFG)).toBe(20);
    // 25% exactly: on 10-10, off 30-30.
    expect(
      targetDutyPct({ onMinMs: 10_000, onMaxMs: 10_000, offMinMs: 30_000, offMaxMs: 30_000 }),
    ).toBe(25);
    // Truncates rather than rounds (matches the entrypoint's shell arithmetic).
    expect(
      targetDutyPct({ onMinMs: 1_000, onMaxMs: 1_000, offMinMs: 2_000, offMaxMs: 3_000 }),
    ).toBe(28);
  });

  it("renders the seconds the operator configured", () => {
    expect(formatCameraCycleConfig(CFG)).toBe("on=[5-15]s off=[20-60]s target_duty=20%");
  });
});

describe("nextPhaseMs", () => {
  it("draws from the phase's own range, inclusive at both ends", () => {
    expect(nextPhaseMs(CFG, "on", 0)).toBe(5_000);
    expect(nextPhaseMs(CFG, "on", 0.999_999_999)).toBe(15_000);
    expect(nextPhaseMs(CFG, "off", 0)).toBe(20_000);
    expect(nextPhaseMs(CFG, "off", 0.999_999_999)).toBe(60_000);
  });

  it("picks the OFF range for the off phase, not the on range", () => {
    expect(nextPhaseMs(CFG, "off", 0.5)).toBeGreaterThanOrEqual(20_000);
  });

  it("stays in range for out-of-contract draws", () => {
    for (const rand of [-1, 1, 2, Number.NaN]) {
      const ms = nextPhaseMs(CFG, "on", rand);
      expect(ms).toBeGreaterThanOrEqual(5_000);
      expect(ms).toBeLessThanOrEqual(15_000);
    }
  });

  it("varies across draws, so two bots do not toggle in lockstep", () => {
    const seen = new Set<number>();
    for (let i = 0; i < 200; i++) seen.add(nextPhaseMs(CFG, "on", Math.random()));
    expect(seen.size).toBeGreaterThan(50);
  });
});

describe("recordCameraPhase", () => {
  it("accumulates into the phase that was running", () => {
    const t = newCameraCycleTally();
    recordCameraPhase(t, "on", 3_000);
    recordCameraPhase(t, "off", 7_000);
    recordCameraPhase(t, "on", 1_000);
    expect(t).toEqual({ confirmed: 0, failed: 0, onMs: 4_000, offMs: 7_000 });
  });

  it("ignores a non-positive or non-finite elapsed", () => {
    const t = newCameraCycleTally();
    for (const ms of [0, -5, Number.NaN, Number.POSITIVE_INFINITY]) recordCameraPhase(t, "on", ms);
    expect(t.onMs).toBe(0);
  });
});

describe("formatCameraCycleReceipt", () => {
  it("reports APPLIED with the observed on-fraction when every toggle confirmed", () => {
    const r = formatCameraCycleReceipt(CFG, {
      confirmed: 6,
      failed: 0,
      onMs: 30_000,
      offMs: 70_000,
    });
    expect(r.banner).toBe(CAMERA_CYCLE_APPLIED_BANNER);
    expect(r.line).toContain("toggles=ok:6/failed:0");
    expect(r.line).toContain("observed_on=30% of 100s");
    expect(r.line).toContain("on=[5-15]s off=[20-60]s target_duty=20%");
  });

  // The receipt-must-not-lie case: a bot whose toggles all failed spent the run
  // camera-on, and must not be indistinguishable from camera-always-on.
  it("reports DEGRADED when any toggle failed, even with confirmed ones", () => {
    const allFailed = formatCameraCycleReceipt(CFG, {
      confirmed: 0,
      failed: 4,
      onMs: 100_000,
      offMs: 0,
    });
    expect(allFailed.banner).toBe(CAMERA_CYCLE_DEGRADED_BANNER);
    expect(allFailed.line).toContain("observed_on=100% of 100s");
    expect(allFailed.line).toContain("not representative");
    const mixed = formatCameraCycleReceipt(CFG, {
      confirmed: 5,
      failed: 1,
      onMs: 50_000,
      offMs: 50_000,
    });
    expect(mixed.banner).toBe(CAMERA_CYCLE_DEGRADED_BANNER);
  });

  it("reports NEVER_FIRED when the run ended before the first boundary", () => {
    const r = formatCameraCycleReceipt(CFG, { confirmed: 0, failed: 0, onMs: 4_000, offMs: 0 });
    expect(r.banner).toBe(CAMERA_CYCLE_NEVER_FIRED_BANNER);
    expect(r.line).toContain("published camera for its whole life");
  });

  it("says n/a rather than inventing a fraction with no measured time", () => {
    const r = formatCameraCycleReceipt(CFG, { confirmed: 0, failed: 0, onMs: 0, offMs: 0 });
    expect(r.line).toContain("observed_on=n/a");
  });
});

interface FakeCall {
  selector: string;
  action: string;
}

/**
 * Playwright-shaped fake. `visible` decides which selectors report visible;
 * `attachFails` makes the post-condition `waitFor` reject for a selector.
 */
function fakePage(opts: {
  visible?: (sel: string) => boolean;
  hoverThrows?: boolean;
  clickThrows?: boolean;
  attachFails?: (sel: string) => boolean;
}): { page: CameraCyclePage; calls: FakeCall[] } {
  const calls: FakeCall[] = [];
  const visible = opts.visible ?? ((): boolean => true);
  const attachFails = opts.attachFails ?? ((): boolean => false);
  const page: CameraCyclePage = {
    locator(selector: string) {
      const self = {
        first: () => self,
        hover: async (): Promise<void> => {
          calls.push({ selector, action: "hover" });
          if (opts.hoverThrows) throw new Error("hover timeout");
        },
        isVisible: async (): Promise<boolean> => {
          calls.push({ selector, action: "isVisible" });
          return visible(selector);
        },
        click: async (): Promise<void> => {
          calls.push({ selector, action: "click" });
          if (opts.clickThrows) throw new Error("click timeout");
        },
        waitFor: async (): Promise<void> => {
          calls.push({ selector, action: "waitFor" });
          if (attachFails(selector)) throw new Error("waitFor attached timeout");
        },
      };
      return self;
    },
  };
  return { page, calls };
}

const STOP_SEL = cameraButtonSelector("Stop Video");
const START_SEL = cameraButtonSelector("Start Video");

describe("setCameraEnabled", () => {
  it("clicks the current-state button and confirms the opposite tooltip", async () => {
    const { page, calls } = fakePage({});
    await expect(setCameraEnabled(page, "off", 1_000)).resolves.toEqual({ ok: true });
    expect(calls).toEqual([
      { selector: ACTION_BAR_SELECTOR, action: "hover" },
      { selector: STOP_SEL, action: "isVisible" },
      { selector: STOP_SEL, action: "click" },
      { selector: START_SEL, action: "waitFor" },
    ]);
  });

  it("clicks the Start Video button when turning the camera back on", async () => {
    const { page, calls } = fakePage({});
    await expect(setCameraEnabled(page, "on", 1_000)).resolves.toEqual({ ok: true });
    expect(calls.map((c) => c.selector)).toEqual([
      ACTION_BAR_SELECTOR,
      START_SEL,
      START_SEL,
      STOP_SEL,
    ]);
  });

  it("re-hovers the auto-hiding action bar on every call", async () => {
    const { page, calls } = fakePage({});
    await setCameraEnabled(page, "off", 1_000);
    await setCameraEnabled(page, "on", 1_000);
    expect(calls.filter((c) => c.selector === ACTION_BAR_SELECTOR)).toHaveLength(2);
  });

  it("still toggles when the hover itself fails", async () => {
    const { page } = fakePage({ hoverThrows: true });
    await expect(setCameraEnabled(page, "off", 1_000)).resolves.toEqual({ ok: true });
  });

  it("fails, rather than reporting success, when the button is not visible", async () => {
    const { page, calls } = fakePage({ visible: () => false });
    const r = await setCameraEnabled(page, "off", 1_000);
    expect(r.ok).toBe(false);
    expect(r.reason).toContain("Stop Video");
    expect(calls.some((c) => c.action === "click")).toBe(false);
  });

  // The post-condition is the whole point: a click that lands off-target leaves
  // the camera unchanged, and fire-and-forget would report it as a toggle.
  it("fails when the tooltip does not flip after the click", async () => {
    const { page, calls } = fakePage({ attachFails: (sel) => sel === START_SEL });
    const r = await setCameraEnabled(page, "off", 1_000);
    expect(r.ok).toBe(false);
    expect(r.reason).toContain("waitFor attached timeout");
    expect(calls.some((c) => c.action === "click")).toBe(true);
  });

  it("fails when the click throws", async () => {
    const { page } = fakePage({ clickThrows: true });
    const r = await setCameraEnabled(page, "off", 1_000);
    expect(r.ok).toBe(false);
    expect(r.reason).toContain("click timeout");
  });

  it("never throws", async () => {
    const boom = {
      locator: vi.fn(() => {
        throw new Error("page closed");
      }),
    } as unknown as CameraCyclePage;
    await expect(setCameraEnabled(boom, "off", 1_000)).resolves.toEqual({
      ok: false,
      reason: "page closed",
    });
  });
});

describe("startCameraCycle", () => {
  const hooks = (): {
    log: string[];
    error: string[];
    log_: (m: string) => void;
    error_: (m: string) => void;
  } => {
    const log: string[] = [];
    const error: string[] = [];
    return { log, error, log_: (m) => log.push(m), error_: (m) => error.push(m) };
  };

  it("alternates on→off→on, drawing each phase from its own range", async () => {
    vi.useFakeTimers();
    try {
      const { page, calls } = fakePage({});
      const h = hooks();
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: h.log_,
        error: h.error_,
        random: () => 0,
      });
      // Nothing happens before the first on-phase (5s) elapses.
      await vi.advanceTimersByTimeAsync(4_999);
      expect(calls.some((c) => c.action === "click")).toBe(false);
      await vi.advanceTimersByTimeAsync(1);
      expect(h.log).toEqual(["camera cycle: camera off"]);
      // Next boundary is the OFF minimum (20s), not the on minimum.
      await vi.advanceTimersByTimeAsync(19_999);
      expect(h.log).toHaveLength(1);
      await vi.advanceTimersByTimeAsync(1);
      expect(h.log).toEqual(["camera cycle: camera off", "camera cycle: camera on"]);
      const receipt = runner.stop();
      expect(receipt.banner).toBe(CAMERA_CYCLE_APPLIED_BANNER);
      expect(receipt.line).toContain("toggles=ok:2/failed:0");
      // 5s on + 20s off + 0s on since the last toggle.
      expect(receipt.line).toContain("observed_on=20% of 25s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps cycling after a failed toggle and reports DEGRADED", async () => {
    vi.useFakeTimers();
    try {
      const { page } = fakePage({ attachFails: () => true });
      const h = hooks();
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: h.log_,
        error: h.error_,
        random: () => 0,
      });
      // Camera stays "on", so every subsequent boundary is the ON minimum.
      await vi.advanceTimersByTimeAsync(5_000);
      await vi.advanceTimersByTimeAsync(5_000);
      expect(h.log).toEqual([]);
      expect(h.error).toHaveLength(2);
      expect(h.error[0]).toContain("FAILED to turn camera off");
      expect(h.error[0]).toContain("NOT being applied");
      const receipt = runner.stop();
      expect(receipt.banner).toBe(CAMERA_CYCLE_DEGRADED_BANNER);
      expect(receipt.line).toContain("toggles=ok:0/failed:2");
      expect(receipt.line).toContain("observed_on=100% of 10s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("accounts a rejected toggle exactly like a reported failure", async () => {
    vi.useFakeTimers();
    try {
      const self: CameraCycleLocator = {
        first: () => self,
        hover: async () => {},
        isVisible: async () => true,
        click: () => Promise.reject(null),
        waitFor: async () => {},
      };
      const page: CameraCyclePage = { locator: () => self };
      const h = hooks();
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: h.log_,
        error: h.error_,
        random: () => 0,
      });
      await vi.advanceTimersByTimeAsync(5_000);
      await vi.advanceTimersByTimeAsync(5_000);
      expect(h.log).toEqual([]);
      expect(h.error).toHaveLength(2);
      expect(h.error[0]).toContain("FAILED to turn camera off");
      const receipt = runner.stop();
      expect(receipt.banner).toBe(CAMERA_CYCLE_DEGRADED_BANNER);
      expect(receipt.line).toContain("toggles=ok:0/failed:2");
      expect(receipt.line).toContain("observed_on=100% of 10s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("rearms after the success reporter throws, and names it an internal fault", async () => {
    vi.useFakeTimers();
    try {
      const { page, calls } = fakePage({});
      const errors: string[] = [];
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: () => {
          throw new Error("log sink closed");
        },
        error: (m) => errors.push(m),
        random: () => 0,
      });
      await vi.advanceTimersByTimeAsync(5_000);
      const clicksAfterFirst = calls.filter((c) => c.action === "click").length;
      expect(clicksAfterFirst).toBe(1);
      await vi.advanceTimersByTimeAsync(20_000);
      expect(calls.filter((c) => c.action === "click").length).toBeGreaterThan(clicksAfterFirst);
      expect(errors.some((m) => m.includes("INTERNAL fault"))).toBe(true);
      expect(errors.some((m) => m.includes("log sink closed"))).toBe(true);
      expect(errors.some((m) => m.includes("FAILED to turn camera"))).toBe(false);
      runner.stop();
    } finally {
      vi.useRealTimers();
    }
  });

  it("rearms even when the error reporter itself is what throws", async () => {
    vi.useFakeTimers();
    try {
      const { page, calls } = fakePage({ attachFails: () => true });
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: () => {},
        error: () => {
          throw new Error("error sink closed");
        },
        random: () => 0,
      });
      await vi.advanceTimersByTimeAsync(5_000);
      const clicksAfterFirst = calls.filter((c) => c.action === "click").length;
      expect(clicksAfterFirst).toBe(1);
      await vi.advanceTimersByTimeAsync(5_000);
      expect(calls.filter((c) => c.action === "click").length).toBeGreaterThan(clicksAfterFirst);
      runner.stop();
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops on stop() — no further toggles, and the receipt is stable", async () => {
    vi.useFakeTimers();
    try {
      const { page, calls } = fakePage({});
      const h = hooks();
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: h.log_,
        error: h.error_,
        random: () => 0,
      });
      await vi.advanceTimersByTimeAsync(5_000);
      const first = runner.stop();
      const clicksAtStop = calls.filter((c) => c.action === "click").length;
      await vi.advanceTimersByTimeAsync(600_000);
      expect(calls.filter((c) => c.action === "click")).toHaveLength(clicksAtStop);
      expect(runner.stop()).toEqual(first);
    } finally {
      vi.useRealTimers();
    }
  });

  it("reports NEVER_FIRED when stopped before the first boundary", async () => {
    vi.useFakeTimers();
    try {
      const { page } = fakePage({});
      const h = hooks();
      const runner = startCameraCycle({
        page,
        config: CFG,
        timeoutMs: 1_000,
        log: h.log_,
        error: h.error_,
        random: () => 0,
      });
      await vi.advanceTimersByTimeAsync(1_000);
      expect(runner.stop().banner).toBe(CAMERA_CYCLE_NEVER_FIRED_BANNER);
    } finally {
      vi.useRealTimers();
    }
  });

  it("draws independently per runner, so two bots do not toggle in lockstep", async () => {
    vi.useFakeTimers();
    try {
      const delays: number[] = [];
      const spy = vi.spyOn(globalThis, "setTimeout");
      for (let i = 0; i < 20; i++) {
        const { page } = fakePage({});
        const h = hooks();
        startCameraCycle({ page, config: CFG, timeoutMs: 1_000, log: h.log_, error: h.error_ });
      }
      for (const call of spy.mock.calls) delays.push(call[1] as number);
      spy.mockRestore();
      expect(new Set(delays).size).toBeGreaterThan(1);
      for (const d of delays) {
        expect(d).toBeGreaterThanOrEqual(CFG.onMinMs);
        expect(d).toBeLessThanOrEqual(CFG.onMaxMs);
      }
    } finally {
      vi.useRealTimers();
    }
  });
});
