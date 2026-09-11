import { describe, expect, it } from "vitest";

import { HEARTBEAT_KEEPALIVE_INTERVAL_MS } from "./rust-mirrored-constants";
import { GlowSample, classifyGlow, summariseGlowTail } from "./speaking-glow";

// The arithmetic behind the untagged `glow-latched-heartbeat.spec.ts`: this runs
// per-PR and pins that the measurement DISCRIMINATES fixed from un-fixed.

const LIT_STYLE =
  "box-shadow: 0 0 24px 4px rgba(45, 226, 230, 0.62), inset 0 0 18px 0px rgba(45, 226, 230, 0.30); " +
  "border-color: rgba(45, 226, 230, 0.85); transition: border-color 0.15s ease-in, box-shadow 0.15s ease-in;";
const SILENT_STYLE =
  "box-shadow: none; border-top-color: var(--grid-item-border); " +
  "border-right-color: var(--grid-item-border); border-bottom-color: var(--grid-item-border); " +
  "border-left-color: var(--grid-item-border); " +
  "transition: border-color 0.3s ease-out 1.00s, box-shadow 1.50s ease-out 1.00s;";

const SAMPLE_INTERVAL_MS = 50;

function record(phases: Array<{ style: string; ms: number }>): GlowSample[] {
  const samples: GlowSample[] = [];
  let at = 0;
  for (const phase of phases) {
    for (let elapsed = 0; elapsed < phase.ms; elapsed += SAMPLE_INTERVAL_MS) {
      samples.push({
        at,
        style: phase.style,
        cls: "",
        muted: "false",
        missing: false,
        marker: null,
      });
      at += SAMPLE_INTERVAL_MS;
    }
  }
  return samples;
}

/** The measurement the spec requires: 3 keepalives, and longer than the 12 500 ms deadman. */
const OBSERVE_MS = Math.max(3 * HEARTBEAT_KEEPALIVE_INTERVAL_MS, 12_500) + 3_000;

describe("classifyGlow", () => {
  it("reads speak_style's two branches, and refuses anything else", () => {
    expect(classifyGlow(LIT_STYLE)).toBe("lit");
    expect(classifyGlow(SILENT_STYLE)).toBe("silent");
    expect(classifyGlow("")).toBe("unknown");
    expect(classifyGlow("transition: border-color 0.2s ease-in, box-shadow 0.2s ease-out;")).toBe(
      "unknown",
    );
  });
});

describe("summariseGlowTail (issue 2660 discriminator)", () => {
  /** FIXED: a later keepalive still claiming `is_speaking = 1` does not re-light
   * it. Phrased against BEHAVIOUR — the fix shape has changed twice. */
  it("reports a full-length tail for a tile that goes dark and stays dark", () => {
    const summary = summariseGlowTail(
      record([
        { style: LIT_STYLE, ms: 3_000 },
        { style: SILENT_STYLE, ms: OBSERVE_MS + 2_000 },
      ]),
    );

    expect(summary.lit).toBeGreaterThan(0);
    expect(summary.muted).toBe(0);
    expect(summary.audioOn).toBe(summary.total);
    expect(summary.unknown).toEqual([]);
    expect(summary.tailMs).toBeGreaterThanOrEqual(OBSERVE_MS);
    expect(summary.darkFrom).toBe((summary.lastLit as GlowSample).at + SAMPLE_INTERVAL_MS);
  });

  /** UN-FIXED: rule 3 re-lights within one keepalive and never releases. The
   * mutation receipt — same summary, same length, collapsed tail. */
  it("collapses the tail when a keepalive relights the tile", () => {
    const summary = summariseGlowTail(
      record([
        { style: LIT_STYLE, ms: 3_000 },
        // Dark until the next heartbeat lands...
        { style: SILENT_STYLE, ms: HEARTBEAT_KEEPALIVE_INTERVAL_MS - 500 },
        // ...then lit for the rest of the window.
        { style: LIT_STYLE, ms: OBSERVE_MS - HEARTBEAT_KEEPALIVE_INTERVAL_MS + 2_500 },
      ]),
    );

    expect(summary.lit).toBeGreaterThan(0);
    expect(summary.tailMs).toBe(0);
    expect(summary.tailSamples).toBe(0);
    expect(summary.tailMs).toBeLessThan(OBSERVE_MS);
  });

  it("measures from the LAST lit sample, so a mid-window blink still fails", () => {
    const summary = summariseGlowTail(
      record([
        { style: LIT_STYLE, ms: 3_000 },
        { style: SILENT_STYLE, ms: 10_000 },
        { style: LIT_STYLE, ms: SAMPLE_INTERVAL_MS },
        { style: SILENT_STYLE, ms: 10_000 },
      ]),
    );

    expect(summary.lit).toBeGreaterThan(1);
    expect(summary.tailMs).toBeLessThan(OBSERVE_MS);
    expect(summary.tailMs).toBeCloseTo(10_000, -2);
  });

  it("reports a tile that never lit, so 'stayed dark' cannot pass trivially", () => {
    const summary = summariseGlowTail(record([{ style: SILENT_STYLE, ms: 20_000 }]));

    expect(summary.lit).toBe(0);
    expect(summary.lastLitIndex).toBe(-1);
    expect(summary.lastLit).toBeUndefined();
    expect(summary.tailSamples).toBe(0);
    expect(summary.tailMs).toBe(0);
    expect(summary.darkFrom).toBe(Number.POSITIVE_INFINITY);
  });

  it("counts the vacuity signals the spec rejects on", () => {
    const samples = record([
      { style: LIT_STYLE, ms: 200 },
      { style: SILENT_STYLE, ms: 200 },
    ]);
    samples[4] = { ...samples[4], muted: "true" };
    samples[5] = { ...samples[5], missing: true, style: "" };

    const summary = summariseGlowTail(samples);
    expect(summary.muted).toBe(1);
    expect(summary.missing).toBe(1);
    expect(summary.unknown).toHaveLength(1);
    expect(summary.audioOn).toBe(summary.total - 1);
  });
});
