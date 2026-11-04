import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  ACTION_BAR_SELECTOR,
  cameraControlSelector,
  MIC_UNMUTE_SELECTOR,
  micControlSelector,
} from "./control-buttons";
import type { ControlTarget } from "./persona";
import {
  type DriverLocator,
  type DriverPage,
  type MediaKind,
  parseHostRender,
  personaReceipt,
  type PersonaTally,
  startPersonaDriver,
} from "./persona-driver";

const ON: Record<MediaKind, string> = {
  mic: micControlSelector("on"),
  camera: cameraControlSelector("on"),
};
const OFF: Record<MediaKind, string> = {
  mic: MIC_UNMUTE_SELECTOR,
  camera: cameraControlSelector("off"),
};

/** The action bar of one meeting page: a click flips the control after `probeMs` (`offProbeMs` for an off click) and logs the Host render line. */
class FakeMeeting implements DriverPage {
  state: Record<MediaKind, boolean> = { mic: false, camera: false };
  clicks: Array<{ kind: MediaKind; at: number }> = [];
  /** A read or click that started while another click had not yet seen its label flip. */
  overlaps = 0;
  reads = 0;
  private settling = false;
  hidden = new Set<MediaKind>();
  renderLines = true;
  constructor(
    private readonly onLine: (text: string) => void,
    private readonly probeMs = 400,
    private readonly offProbeMs = probeMs,
    private readonly readMs = 0,
  ) {}

  private flip(kind: MediaKind): void {
    const prev = { ...this.state };
    this.state[kind] = !this.state[kind];
    if (this.renderLines) {
      this.onLine(
        `Host render: video=${this.state.camera} prev=${prev.camera} mic=${this.state.mic} prev=${prev.mic} screen=false prev=false`,
      );
    }
  }

  locator(selector: string): DriverLocator {
    const kind = (["mic", "camera"] as const).find(
      (k) => selector === ON[k] || selector === OFF[k],
    );
    const showsOn = kind !== undefined && selector === ON[kind];
    const visible = (): boolean =>
      kind !== undefined && !this.hidden.has(kind) && this.state[kind] === showsOn;
    const self: DriverLocator = {
      first: () => self,
      hover: async () => {
        if (selector !== ACTION_BAR_SELECTOR) throw new Error("hovered a control");
      },
      isVisible: async () => {
        this.reads += 1;
        if (this.settling) this.overlaps += 1;
        if (this.readMs > 0) await new Promise((r) => setTimeout(r, this.readMs));
        return visible();
      },
      click: async () => {
        if (!visible() || kind === undefined) throw new Error(`clicked hidden ${selector}`);
        if (this.settling) this.overlaps += 1;
        this.settling = true;
        this.clicks.push({ kind, at: Date.now() });
        setTimeout(() => this.flip(kind), this.state[kind] ? this.offProbeMs : this.probeMs);
      },
      waitFor: async ({ timeout }) => {
        const deadline = Date.now() + (timeout ?? 0);
        while (!visible()) {
          if (Date.now() >= deadline) throw new Error("waitFor timed out");
          await new Promise((r) => setTimeout(r, 50));
        }
        this.settling = false;
      },
    };
    return self;
  }
}

type Plan = (t: number) => Record<MediaKind, ControlTarget>;
const T0 = 1_760_000_000_000;

function drive(
  plan: Plan,
  {
    joined = { mic: false, camera: false },
    probeMs = 400,
    offProbeMs = probeMs,
    tickMs = 250,
    readMs = 0,
    onTally = () => {},
  }: {
    joined?: Record<MediaKind, boolean>;
    probeMs?: number;
    offProbeMs?: number;
    tickMs?: number;
    readMs?: number;
    onTally?: (tally: PersonaTally) => void;
  } = {},
) {
  const logs: string[] = [];
  const errors: string[] = [];
  const tallies: PersonaTally[] = [];
  let driver: ReturnType<typeof startPersonaDriver> | null = null;
  const page = new FakeMeeting((line) => driver?.onConsole(line), probeMs, offProbeMs, readMs);
  page.state = { ...joined };
  driver = startPersonaDriver({
    page,
    targets: plan,
    joined,
    log: (m) => logs.push(m),
    error: (m) => errors.push(m),
    onTally: (t) => {
      tallies.push(t);
      onTally(t);
    },
    settleMs: 2_000,
    tickMs,
  });
  return { page, driver, logs, errors, tallies };
}

/** Mic on during [10 s, 30 s) and [31.5 s, 50 s) after T0; camera on during [12 s, 45 s). */
const window = (on: number, off: number, t: number): ControlTarget =>
  t < on
    ? { on: false, since: -Infinity }
    : t < off
      ? { on: true, since: on }
      : { on: false, since: off };
const twoStretches: Plan = (t) => ({
  mic: t < T0 + 31_500 ? window(T0 + 10_000, T0 + 30_000, t) : window(T0 + 31_500, T0 + 50_000, t),
  camera: window(T0 + 12_000, T0 + 45_000, t),
});

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
});
afterEach(() => vi.useRealTimers());

describe("startPersonaDriver", () => {
  it("follows the plan, timing each toggle from its Host render line", async () => {
    const { page, driver, logs } = drive(twoStretches);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(page.state).toEqual({ mic: false, camera: false });
    const tally = driver.stop();
    expect(page.clicks.map((c) => c.kind)).toEqual([
      "mic",
      "camera",
      "mic",
      "mic",
      "camera",
      "mic",
    ]);
    expect(tally).toMatchObject({ ok: 6, failed: 0, already: 0, rendered: 6, unrendered: 0 });
    expect(tally.medianAbsMs).toBeGreaterThanOrEqual(400);
    expect(tally.medianAbsMs).toBeLessThan(1_000);
    expect(tally.minGapMs).toBeGreaterThanOrEqual(1_000);
    expect(tally.minGapMs).toBeLessThan(2_000);
    expect(logs.some((l) => /^persona: mic on rendered .* \(\d+ ms after plan\)$/.test(l))).toBe(
      true,
    );
  });

  it("never clicks one control twice within 1 s, even when the plan flaps", async () => {
    const flapping: Plan = (t) => {
      const on = Math.floor((t - T0) / 300) % 2 === 1;
      return { mic: { on, since: t }, camera: { on, since: t } };
    };
    const { page, driver } = drive(flapping, { probeMs: 50 });
    await vi.advanceTimersByTimeAsync(20_000);
    driver.stop();
    for (const kind of ["mic", "camera"] as const) {
      const at = page.clicks.filter((c) => c.kind === kind).map((c) => c.at);
      expect(at.length).toBeGreaterThan(5);
      for (let k = 1; k < at.length; k += 1)
        expect(at[k] - at[k - 1]).toBeGreaterThanOrEqual(1_000);
    }
  });

  it("keeps one read or click in flight at a time", async () => {
    const both: Plan = (t) => ({
      mic: window(T0 + 1_000, T0 + 9_000, t),
      camera: window(T0 + 1_000, T0 + 9_000, t),
    });
    const { page, driver } = drive(both);
    await vi.advanceTimersByTimeAsync(15_000);
    driver.stop();
    expect(page.clicks.length).toBe(4);
    expect(page.overlaps).toBe(0);
  });

  it("reads the control first and does not click one already in the planned state", async () => {
    const { page, driver } = drive(twoStretches);
    page.state.mic = true;
    await vi.advanceTimersByTimeAsync(11_000);
    expect(page.clicks).toEqual([]);
    expect(driver.stop()).toMatchObject({ ok: 0, already: 1 });
  });

  it("fails a missing control's toggle once while its planned edge stays the same", async () => {
    const { page, driver, errors } = drive(twoStretches);
    page.hidden.add("camera");
    await vi.advanceTimersByTimeAsync(40_000);
    const tally = driver.stop();
    expect(tally.failed).toBe(1);
    expect(errors).toEqual([
      expect.stringContaining("persona: FAILED camera on (no visible control)"),
    ]);
    expect(personaReceipt("mic=push-to-talk camera=with-voice", tally)).toMatchObject({
      degraded: true,
      line: expect.stringMatching(
        /^PERSONA_DEGRADED mic=push-to-talk camera=with-voice toggles=ok:3\/failed:1\/already:0 /,
      ),
    });
  });

  it("counts a clicked toggle with no Host render line as unrendered, not as timed", async () => {
    const { page, driver } = drive(twoStretches);
    page.renderLines = false;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(driver.stop()).toMatchObject({ ok: 6, rendered: 0, unrendered: 6, medianAbsMs: null });
  });

  it("does not time the first reconciliation after join", async () => {
    const started: Plan = (t) => ({
      mic: { on: true, since: T0 - 5_000 },
      camera: window(T0 + 2_000, T0 + 60_000, t),
    });
    const { driver, logs } = drive(started);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(logs).toContainEqual(expect.stringMatching(/^persona: mic on initial clicked /));
    expect(logs).toContainEqual(expect.stringMatching(/^persona: mic on rendered [\d:.]+$/));
    expect(driver.stop()).toMatchObject({ ok: 2, rendered: 2 });
  });
});

describe("startPersonaDriver edge cases", () => {
  it("clicks one control again exactly 1000 ms after its last click, not 999", async () => {
    const onThenOff: Plan = (t) => ({
      mic: t < T0 + 100 ? { on: true, since: T0 } : { on: false, since: T0 + 100 },
      camera: { on: false, since: -Infinity },
    });
    const { page, driver } = drive(onThenOff, { probeMs: 0, tickMs: 1 });
    await vi.advanceTimersByTimeAsync(3_000);
    driver.stop();
    expect(page.clicks.map((c) => c.at - page.clicks[0].at)).toEqual([0, 1_000]);
  });

  it("re-reads a control whose label flipped after the settle timeout before the next edge", async () => {
    const stretch: Plan = (t) => ({
      mic: window(T0 + 1_000, T0 + 10_000, t),
      camera: { on: false, since: -Infinity },
    });
    const { page, driver } = drive(stretch, { probeMs: 3_000, offProbeMs: 100 });
    await vi.advanceTimersByTimeAsync(15_000);
    expect(page.state.mic).toBe(false);
    expect(page.clicks.map((c) => c.kind)).toEqual(["mic", "mic"]);
    expect(driver.stop()).toMatchObject({ ok: 1, failed: 1 });
  });

  it.each([
    ["a click that settles", { probeMs: 400 }],
    ["a click that times out", { probeMs: 3_000 }],
    ["a control read", { readMs: 500 }],
  ])("changes nothing after stop() when %s resolves later", async (_what, opts) => {
    const stretch: Plan = (t) => ({
      mic: window(T0 + 1_000, T0 + 60_000, t),
      camera: { on: false, since: -Infinity },
    });
    const { page, driver, logs, errors, tallies } = drive(stretch, opts);
    await vi.advanceTimersByTimeAsync(1_200);
    const atStop = driver.stop();
    const seen = {
      tallies: tallies.length,
      logs: logs.length,
      errors: errors.length,
      clicks: page.clicks.length,
    };
    await vi.advanceTimersByTimeAsync(10_000);
    expect(driver.stop()).toEqual(atStop);
    expect({
      tallies: tallies.length,
      logs: logs.length,
      errors: errors.length,
      clicks: page.clicks.length,
    }).toEqual(seen);
  });

  it("reads no control once stopped, so a hang-up that hides them records no failure", async () => {
    const { page, driver } = drive(twoStretches);
    await vi.advanceTimersByTimeAsync(11_000);
    const atStop = driver.stop();
    const reads = page.reads;
    page.hidden.add("mic");
    page.hidden.add("camera");
    await vi.advanceTimersByTimeAsync(50_000);
    expect(page.reads).toBe(reads);
    expect(driver.stop()).toEqual(atStop);
    expect(atStop.failed).toBe(0);
  });

  it("retries a failed toggle once its planned edge is out of reach (since -Infinity)", async () => {
    const plan: Plan = (t) => ({
      mic: t < T0 + 20_000 ? window(T0 + 1_000, T0 + 5_000, t) : { on: false, since: -Infinity },
      camera: { on: false, since: -Infinity },
    });
    const { page, driver } = drive(plan, { offProbeMs: 100 });
    await vi.advanceTimersByTimeAsync(4_900);
    page.hidden.add("mic");
    await vi.advanceTimersByTimeAsync(5_000);
    page.hidden.delete("mic");
    await vi.advanceTimersByTimeAsync(9_000);
    expect(page.state.mic).toBe(true);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(page.state.mic).toBe(false);
    expect(driver.stop()).toMatchObject({ ok: 2, failed: 1 });
  });

  it("times a toggle from its own Host render edge, not an opposite one", async () => {
    const stretch: Plan = (t) => ({
      mic: window(T0 + 1_000, T0 + 60_000, t),
      camera: { on: false, since: -Infinity },
    });
    const { driver } = drive(stretch, { probeMs: 400 });
    await vi.advanceTimersByTimeAsync(1_100);
    driver.onConsole(
      "Host render: video=false prev=false mic=false prev=true screen=false prev=false",
    );
    await vi.advanceTimersByTimeAsync(1_000);
    const tally = driver.stop();
    expect(tally.rendered).toBe(1);
    expect(tally.medianAbsMs).toBeGreaterThanOrEqual(400);
  });

  it("takes the lower middle as the median of an even count", async () => {
    const stretch: Plan = (t) => ({
      mic: window(T0 + 1_000, T0 + 5_000, t),
      camera: { on: false, since: -Infinity },
    });
    const { driver, logs } = drive(stretch, { probeMs: 400, offProbeMs: 100 });
    await vi.advanceTimersByTimeAsync(10_000);
    const offsets = logs.flatMap((l) => /\((\d+) ms after plan\)$/.exec(l)?.[1] ?? []).map(Number);
    expect(offsets).toHaveLength(2);
    expect(offsets[0]).not.toBe(offsets[1]);
    expect(driver.stop().medianAbsMs).toBe(Math.min(...offsets));
  });

  it("keeps toggling and reports it when the tally sink throws", async () => {
    const { page, driver, errors } = drive(twoStretches, {
      onTally: () => {
        throw new Error("disk full");
      },
    });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(page.clicks).toHaveLength(6);
    expect(errors).toContainEqual(
      expect.stringContaining("persona: tally sink failed (Error: disk full)"),
    );
    driver.stop();
  });

  it("keeps the first reconciliation after join out of the median", async () => {
    const openMic: Plan = () => ({
      mic: { on: true, since: T0 - 5_000 },
      camera: { on: false, since: -Infinity },
    });
    const { driver } = drive(openMic);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(driver.stop()).toMatchObject({ ok: 1, rendered: 1, medianAbsMs: null });
  });

  it("never retries a fixed persona's failed join toggle", async () => {
    const openMic: Plan = () => ({
      mic: { on: true, since: -Infinity },
      camera: { on: false, since: -Infinity },
    });
    const { page, driver } = drive(openMic);
    page.hidden.add("mic");
    await vi.advanceTimersByTimeAsync(2_000);
    page.hidden.delete("mic");
    await vi.advanceTimersByTimeAsync(30_000);
    expect(page.clicks).toEqual([]);
    expect(driver.stop()).toMatchObject({ ok: 0, failed: 1 });
  });
});

describe("parseHostRender", () => {
  it("returns only the media whose state changed", () => {
    expect(
      parseHostRender(
        "Host render: video=true prev=true mic=true prev=false screen=false prev=false",
      ),
    ).toEqual([{ kind: "mic", on: true }]);
    expect(parseHostRender("Host render: camera ON, auto-select device_id=''")).toEqual([]);
  });
});
