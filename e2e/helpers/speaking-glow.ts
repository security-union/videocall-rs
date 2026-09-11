// Recording and classifying a peer tile's speaking glow. Extracted verbatim from
// `tests/speaking-glow-mute-veto.spec.ts` so both specs share one classifier.

import { Page } from "@playwright/test";

export type GlowVerdict = "lit" | "silent" | "unknown";

export interface GlowSample {
  at: number;
  style: string;
  cls: string;
  /** Read in-page at 50ms so the deadline's zero point is not moved past the
   * window under test. `null` on synthetic `oldValue` entries. */
  muted: string | null;
  missing: boolean;
  marker: string | null;
}

interface GlowWindow {
  __vcGlowSamples?: GlowSample[];
  __vcGlowMark?: (label: string) => void;
  __vcGlowStop?: () => void;
}

/**
 * Keyed on transition easing — the one property `speak_style`'s two branches
 * never share. Colour is themed and `box-shadow: none` appears in both.
 * Anything else is `"unknown"`, never folded into a verdict.
 */
export function classifyGlow(style: string): GlowVerdict {
  const lit = style.includes("ease-in");
  const silent = style.includes("ease-out");
  if (lit === silent) {
    return "unknown";
  }
  return lit ? "lit" : "silent";
}

/**
 * Observer keeps each mutation's `oldValue` so a sub-microtask blink survives;
 * the 50ms poll re-queries the id so a rebuilt tile is still tracked. THROWS
 * without `#grid-container`: interval-only would pass every non-vacuity check.
 */
export async function startGlowTimeline(page: Page, tileId: string): Promise<void> {
  await page.evaluate((id) => {
    const w = window as unknown as GlowWindow;
    const samples: GlowSample[] = [];
    w.__vcGlowSamples = samples;

    const sample = (marker: string | null) => {
      const el = document.getElementById(id);
      samples.push({
        at: performance.now(),
        style: el?.getAttribute("style") || "",
        cls: el?.getAttribute("class") || "",
        muted: el?.querySelector("[data-mic-muted]")?.getAttribute("data-mic-muted") ?? null,
        missing: el === null,
        marker,
      });
    };

    sample(null);

    const container = document.getElementById("grid-container");
    if (!container) {
      throw new Error(
        "#grid-container not found — cannot install the glow MutationObserver, so a sub-poll " +
          "re-light would go unrecorded and a pass would be meaningless",
      );
    }

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.attributeName === "style" && (record.target as Element).id === id) {
          samples.push({
            at: performance.now(),
            style: record.oldValue || "",
            cls: "",
            muted: null,
            missing: false,
            marker: null,
          });
        }
      }
      sample(null);
    });
    observer.observe(container, {
      subtree: true,
      attributes: true,
      attributeOldValue: true,
      attributeFilter: ["style", "class", "data-mic-muted"],
    });

    const timer = window.setInterval(() => sample(null), 50);

    w.__vcGlowMark = (label: string) => sample(label);
    w.__vcGlowStop = () => {
      observer.disconnect();
      window.clearInterval(timer);
    };
  }, tileId);
}

export async function markGlowTimeline(page: Page, label: string): Promise<void> {
  await page.evaluate((l) => {
    const w = window as unknown as GlowWindow;
    if (!w.__vcGlowMark) {
      throw new Error(`glow timeline not running — cannot mark "${l}"`);
    }
    w.__vcGlowMark(l);
  }, label);
}

export async function stopGlowTimeline(page: Page): Promise<GlowSample[]> {
  return page.evaluate(() => {
    const w = window as unknown as GlowWindow;
    w.__vcGlowStop?.();
    return w.__vcGlowSamples ?? [];
  });
}

export function describeSample(s: GlowSample | undefined, zero = 0): string {
  if (!s) {
    return "n/a";
  }
  return `at=+${(s.at - zero).toFixed(0)}ms verdict=${classifyGlow(s.style)} style="${s.style}"`;
}

/** Pure, so the arithmetic behind the untagged browser spec runs in per-PR CI. */
export interface GlowTailSummary {
  total: number;
  missing: number;
  unknown: GlowSample[];
  muted: number;
  audioOn: number;
  lit: number;
  lastLitIndex: number;
  lastLit?: GlowSample;
  tailSamples: number;
  /** Last glow to end of recording — a re-light moves `lastLitIndex` forward so
   * the tail COLLAPSES rather than showing a tolerable blemish. */
  tailMs: number;
  /** `+Infinity` when never dark, so a caller filtering on it gets an empty set. */
  darkFrom: number;
}

export function summariseGlowTail(samples: GlowSample[]): GlowTailSummary {
  let lastLitIndex = -1;
  let lit = 0;
  const unknown: GlowSample[] = [];
  let missing = 0;
  let muted = 0;
  let audioOn = 0;

  samples.forEach((s, i) => {
    switch (classifyGlow(s.style)) {
      case "lit":
        lit += 1;
        lastLitIndex = i;
        break;
      case "unknown":
        unknown.push(s);
        break;
      default:
        break;
    }
    if (s.missing) {
      missing += 1;
    }
    if (s.muted === "true") {
      muted += 1;
    } else if (s.muted === "false") {
      audioOn += 1;
    }
  });

  const tail = lastLitIndex >= 0 ? samples.slice(lastLitIndex + 1) : [];
  return {
    total: samples.length,
    missing,
    unknown,
    muted,
    audioOn,
    lit,
    lastLitIndex,
    lastLit: lastLitIndex >= 0 ? samples[lastLitIndex] : undefined,
    tailSamples: tail.length,
    tailMs: tail.length > 0 ? tail[tail.length - 1].at - samples[lastLitIndex].at : 0,
    darkFrom: tail.length > 0 ? tail[0].at : Number.POSITIVE_INFINITY,
  };
}
