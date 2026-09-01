import { readFileSync } from "node:fs";
import path from "node:path";

import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Issue 2225 — the speaking-glow deadman must be RE-ARMED at most once per
 * throttle period, not once per speech event.
 *
 * WHAT CHANGED (the behaviour this file pins)
 * -------------------------------------------
 * `arm_glow_deadman` (`dioxus-ui/src/components/peer_tile.rs`) allocates a boxed
 * `Closure`, a wasm-bindgen heap slot and a `setTimeout`, and cancels the
 * previous one by dropping it — and before 2225 it did that on EVERY event that
 * counts as evidence of speech (`refreshes_glow_deadman`). A talking peer emits
 * those at the NetEQ worker's PCM rate, not at the heartbeat rate. 2225 adds
 * `admit_glow_deadman_rearm`, which suppresses a re-arm while a timer is already
 * pending and younger than `GLOW_DEADMAN_REARM_THROTTLE_MS`, collapsing the
 * churn to at most one create/destroy pair per period.
 *
 * HOW THIS SPEC OBSERVES IT
 * -------------------------
 * The deadman is a `gloo_timers::callback::Timeout`, which bottoms out in a
 * plain global `setTimeout(handler, 12500)`: gloo-timers 0.2.6 declares
 * `#[wasm_bindgen(js_name = "setTimeout", catch)]`, and the generated glue calls
 * the FREE identifier (`const ret = setTimeout(arg0, arg1);` — verified in the
 * built `dioxus-ui/dist/videocall-ui-*.js`), so it resolves `setTimeout` off the
 * global object at CALL time. An `addInitScript` wrapper installed on the host
 * context before any page script therefore sees every arm.
 *
 * The 12 500 ms delay is a safe filter: it is the ONLY timer of that duration in
 * the app. `GLOW_DEADMAN_MS = glow_deadman_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS)`
 * = 5000 * 5 / 2 (`videocall-aq/src/constants.rs`), and every other
 * `Timeout::new` in `dioxus-ui/` and `videocall-client/` uses 0, 100, 500, 1000,
 * 4 000, 6 000, 8 000 or a named constant of a different value. Nothing in the
 * page's JavaScript (`dioxus-ui/scripts/`) schedules 12 500 ms either.
 *
 * WHY IT DISCRIMINATES (the arithmetic)
 * -------------------------------------
 * The guest's fake mic runs `continuousToneWavPath()` — a swept-amplitude tone
 * (`helpers/audio-fixtures.ts`): peak 0.6 / floor 0.085 amplitude on a 2 s
 * envelope. RMS is amplitude/√2, and `rms_to_intensity` saturates at
 * `RMS_LOUD_SPEECH_CEILING` = 0.10, so intensity is pinned at 1.0 for most of
 * the envelope and sweeps 1.0 → ~0.71 → 1.0 through each trough. The decoder-side
 * VAD re-broadcasts whenever intensity moves more than
 * `AUDIO_LEVEL_DELTA_THRESHOLD` (0.02), i.e. ~29 qualifying events per 2 s
 * envelope — roughly 200 across this spec's 15 s window.
 *
 *   - UN-FIXED: one `setTimeout(_, 12500)` per qualifying event → ~200 arms in
 *     the window, an order of magnitude over [`ARM_BUDGET`] (17).
 *   - FIXED: at most one arm per 1 000 ms period → 8–15 arms, inside the budget.
 *
 * [`MIN_STYLE_WRITES`] and the `styleWrites >= 3 * arms` check make that argument
 * self-calibrating rather than a bet on the event rate a given machine achieves.
 * The tile's inline `style` attribute is EXACTLY `speak_style(...)`
 * (`canvas_generator.rs`: `style: "{grid_tile_style}"`), and Dioxus writes it
 * only when the rendered string changes, so each style mutation implies at least
 * one level change.
 *
 * THE SAFETY HALF
 * ---------------
 * A throttle is only correct if it cannot become a mute button. The observation
 * window is 15 s — longer than the 12 500 ms deadman — so the timer pending when
 * recording starts MUST fire inside it unless something re-armed. A mutation that
 * suppressed the re-arm unconditionally (dropping the elapsed check, or dropping
 * the `timer_pending` clause after `apply_resolved_level` takes the deadman)
 * therefore drives a still-talking peer's glow dark inside the window, and the
 * "no silent sample" assertion goes red. That is the issue-2174 stuck/dropped
 * glow class of defect, re-entered through a performance fix.
 *
 * WHAT THIS FILE DELIBERATELY DOES NOT COVER
 * ------------------------------------------
 * The other half of 2225 — per-kind decoder resets (`reset_for_decode_error`
 * rebuilding only the decoder a `PeerDecodeError` implicates) and the widened
 * keyframe-route self-heal probe — is NOT reachable from a browser test. The
 * only `PeerDecodeError`s a real packet can produce are pre-decode ones
 * (`AesDecryptError`, `PacketParseError`, `NoMediaType`, `NoPacketType`,
 * `IncorrectPacketType`, `Unknown*`), and every one of them maps to
 * `DecoderResetScope::None` — no decoder is rebuilt, so no route is dropped and
 * there is nothing for a spec to observe. The variants that DO rebuild a decoder
 * (`VideoDecodeError` / `ScreenDecodeError` / `AudioDecodeError`) are produced
 * only by `map_err` on a `decoder.decode(..)` call, and neither concrete decoder
 * has an `Err` path: `VideoPeerDecoder::decode` returns `Ok` on both of its
 * returns (`peer_decoder.rs`), and `create_audio_peer_decoder` always builds the
 * NetEQ decoder, whose `decode` also returns `Ok` on both arms. That half is
 * guarded by the wasm-bindgen browser test
 * `decode_reinstalls_a_screen_only_route_drop` in `peer_decode_manager.rs`,
 * which drives the real `decode()` with a screen-only route drop.
 *
 * ISSUE 2289 — THE TILE'S AUDIO-LEVEL WRITE GATE (second describe block)
 * WHAT CHANGED: `UI_AUDIO_LEVEL_DELTA` (`videocall-client/src/audio_constants.rs`)
 * became `AUDIO_LEVEL_DELTA_THRESHOLD * 2.0`, 0.01 → 0.04. Each assertion is labelled
 * DISCRIMINATOR / CORROBORATING / SANITY FLOOR at its call site and states in its own
 * `expect()` message why it fails on un-fixed code, over the [`RENDER_CHAIN`] trace.
 *
 * TAGGING: both tests are deliberately UNTAGGED (no `@bvt0` / `@bvt1`), same reason as
 * `speaking-glow-mute-veto.spec.ts`: two real browsers, a full join dance with a
 * fake microphone, and a 15 s observation window — ~90 s each. They therefore do NOT
 * run in per-PR CI and must be validated by a full `--project=dioxus` run (local
 * `make e2e`, or a scoped `/run-e2e dioxus` dispatch).
 */

// ---------------------------------------------------------------------------
// Constants — every one traced to the production source it mirrors
// ---------------------------------------------------------------------------

/**
 * The deadman's `setTimeout` delay, and therefore the filter this spec keys on.
 *
 * `peer_tile.rs`: `GLOW_DEADMAN_MS = glow_deadman_ms(HEARTBEAT_KEEPALIVE_INTERVAL_MS)`
 * where `glow_deadman_ms(k) = k * 5 / 2` and the keepalive is 5 000 ms
 * (`videocall-aq/src/constants.rs`). If either changes, the arms this spec
 * counts drop to zero and the `at least one arm` assertion below fails loudly
 * rather than passing on an empty set.
 */
const GLOW_DEADMAN_MS = 12_500;

/**
 * `peer_tile.rs`: `glow_deadman_rearm_throttle_ms(k) = k / 5` → 1 000 ms at the
 * shipped keepalive. This is the period the fix collapses a burst of speech
 * events into a single timer rebuild.
 */
const GLOW_DEADMAN_REARM_THROTTLE_MS = 1_000;

/**
 * How long the tile is watched.
 *
 * Chosen to exceed [`GLOW_DEADMAN_MS`]: the timer pending when recording starts
 * fires inside this window unless a re-arm replaced it, which is what makes the
 * "the glow never went dark" assertion a real guard on the throttle's exit path
 * rather than a restatement of "the peer was speaking".
 */
const OBSERVE_MS = 15_000;

/**
 * The most arms a CORRECT build may perform in the window.
 *
 * One per throttle period, plus two for the boundaries: the window can open and
 * close mid-period, and `js_sys::Date::now()` (which the throttle reads) is a
 * wall clock that need not tick in lockstep with the timestamps recorded here.
 */
const ARM_BUDGET = Math.ceil(OBSERVE_MS / GLOW_DEADMAN_REARM_THROTTLE_MS) + 2;

/**
 * The absolute non-vacuity floor on level updates: 2 per second across the
 * window.
 *
 * The fixture's arithmetic predicts ~14 qualifying events per second, so this is
 * a ~7x margin — but a run that produced only a handful (a dead fake mic, a
 * stalled audio path) must fail rather than pass a throttle assertion that is
 * trivially true when nothing was arriving to throttle.
 */
const MIN_STYLE_WRITES = Math.round((2 * OBSERVE_MS) / 1_000);

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const AUDIO_CONSTANTS_RS = "videocall-client/src/audio_constants.rs";
const CANVAS_GENERATOR_RS = "dioxus-ui/src/components/canvas_generator.rs";

function rustSource(rel: string): string {
  const abs = path.resolve(REPO_ROOT, rel);
  try {
    return readFileSync(abs, "utf8");
  } catch (err) {
    throw new Error(
      `cannot read ${rel} (resolved ${abs}); this spec reads its blur-step floor out of ` +
        `that file at load time rather than mirroring it`,
      { cause: err },
    );
  }
}

function rustF32Literal(rel: string, name: string): number {
  const re = new RegExp(`^(?:pub )?const ${name}\\s*:\\s*f32\\s*=\\s*([^;]+);`, "gm");
  const hits = [...rustSource(rel).matchAll(re)];
  if (hits.length !== 1) {
    throw new Error(
      `expected exactly 1 top-level \`const ${name}: f32\` in ${rel}, found ${hits.length} — ` +
        `renamed, moved or redefined; re-point this derivation at the new source`,
    );
  }
  const raw = hits[0][1].trim().replace(/_/g, "");
  if (!/^-?\d+(\.\d+)?$/.test(raw)) {
    throw new Error(
      `\`const ${name}\` in ${rel} is \`${raw}\`, not a plain number literal; this reader ` +
        `only resolves literals, so ${name} must be derived explicitly here`,
    );
  }
  return Number(raw);
}

function uiAudioLevelDeltaFromRust(): number {
  const multiplier = rustSource(AUDIO_CONSTANTS_RS).match(
    /^pub const UI_AUDIO_LEVEL_DELTA\s*:\s*f32\s*=\s*AUDIO_LEVEL_DELTA_THRESHOLD\s*\*\s*([\d.]+)\s*;/m,
  );
  if (!multiplier) {
    throw new Error(
      `UI_AUDIO_LEVEL_DELTA in ${AUDIO_CONSTANTS_RS} is no longer ` +
        `\`AUDIO_LEVEL_DELTA_THRESHOLD * <literal>\`; re-derive it before trusting ` +
        `MIN_RENDERED_BLUR_STEP_PX, which is only a discriminator while it exceeds ` +
        `the blur a single admitted level move produces`,
    );
  }
  return rustF32Literal(AUDIO_CONSTANTS_RS, "AUDIO_LEVEL_DELTA_THRESHOLD") * Number(multiplier[1]);
}

const UI_AUDIO_LEVEL_DELTA = uiAudioLevelDeltaFromRust();

const SEEDED_GLOW_BLEED = rustF32Literal(CANVAS_GENERATOR_RS, "GLOW_BLEED_OLD_MAX");
const BLUR_PX_PER_LEVEL =
  rustF32Literal(CANVAS_GENERATOR_RS, "OUTER_BLUR_INTENSITY") * SEEDED_GLOW_BLEED;

const MIN_RENDERED_BLUR_STEP_PX = Math.floor(UI_AUDIO_LEVEL_DELTA * BLUR_PX_PER_LEVEL - 1) + 1;
const ONE_PX_STEP_BUDGET = 2;
const FIXTURE_LEVEL_TRAVEL_PER_ENVELOPE = 2 * (1.0 - 0.708);
const FIXTURE_ENVELOPE_MS = 2_000;

const RENDER_CHAIN = [
  "level -> tile style, traced in production source:",
  "  peer_tile.rs:1487         if glow_write_reaches_signal(lvl, prev) { audio_level.set(lvl) }",
  "  peer_tile.rs:1021         (lvl == 0.0 && prev != 0.0) || ui_level_write_is_visible(lvl, prev)",
  "  canvas_generator.rs:961   speak_style(visible_audio_level, is_speaking, appearance)",
  '  canvas_generator.rs:1708  style: "{grid_tile_style}" on the tile ROOT div',
  "  canvas_generator.rs:196   glowing branch emits box-shadow: 0 0 {outer_blur:.0}px ...",
  "  canvas_generator.rs:144   outer_blur = OUTER_BLUR_BASE + i * OUTER_BLUR_INTENSITY * glow_bleed",
  "  context.rs:500-501 + canvas_generator.rs:293/:57/:59 at the seeded appearance make that",
  `  14 + ${BLUR_PX_PER_LEVEL.toFixed(1)} * level, quantised to whole px by {:.0}`,
].join("\n");

const MAX_STYLE_WRITES = Math.ceil(
  (FIXTURE_LEVEL_TRAVEL_PER_ENVELOPE * (OBSERVE_MS / FIXTURE_ENVELOPE_MS)) / UI_AUDIO_LEVEL_DELTA,
);

const MIN_RENDERED_STYLES = 30;
const MIN_NONZERO_STEPS = 20;
const MIN_PEAK_BLUR_PX = 45;

// PLAIN text, not JSON: `context.rs::load_appearance_settings_from_storage` reads
// these keys once, at `attendants.rs`'s mount, via bare `f32::to_string()` / `"true"`.
const APPEARANCE_SEED_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_appearance_glow_enabled", "true");
    localStorage.setItem("vc_appearance_inner_glow_strength", "0.5");
    localStorage.setItem("vc_appearance_glow_brightness", "0.5");
  } catch (_) {}
})();`;

// ---------------------------------------------------------------------------
// In-page probe types (no runtime values cross the evaluate boundary)
// ---------------------------------------------------------------------------

/** Every `setTimeout` the page made, plus the timestamps of the deadman ones. */
interface TimerProbe {
  total: number;
  deadmanArms: number[];
}

/** One recorded state of the tracked tile. */
interface GlowSample {
  at: number;
  style: string;
  missing: boolean;
}

interface ChurnRecorder {
  samples: GlowSample[];
  styleWrites: number;
  /** Rendered `style` values IN ORDER: `oldValue`s closed by the value held at stop, so
   * two writes in one microtask batch — which a DOM re-read collapses — both survive. */
  styleTrack: string[];
  stop: () => void;
}

interface ChurnWindow {
  __vcTimerProbe?: TimerProbe;
  __vcGlowChurn?: ChurnRecorder;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Classify a tile's inline style as `speak_style`'s GLOWING or SILENT output.
 *
 * Keyed on the transition EASING — the one property the two branches never share
 * (`canvas_generator.rs::speak_style`): both silent branches emit `ease-out` and
 * never `ease-in`, both glowing branches the reverse. Colour literals are themed
 * and `box-shadow: none` is emitted by a glowing tile too when
 * `inner_glow_strength` is 0, so neither can stand alone. Anything that is
 * neither — an empty attribute, a detached node, a future style — is `unknown`
 * and never folded into a verdict; the assertions below reject `unknown` rather
 * than counting it as evidence in whichever direction happens to suit.
 */
function classifyGlow(style: string): "lit" | "silent" | "unknown" {
  const lit = style.includes("ease-in");
  const silent = style.includes("ease-out");
  if (lit === silent) {
    return "unknown";
  }
  return lit ? "lit" : "silent";
}

/** The dock's mic toggle, selected by the stable testid it carries. */
function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

/** Turn the local microphone on. The mic starts muted in the E2E stack. */
async function enableMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

/**
 * Bring the host's view of the guest to a settled, glowing state.
 *
 * Presence before measurement: the tile must exist, must be the ONLY remote tile
 * (the local self-view is the `Host` component, not a `.grid-item`, so a second
 * one would mean this spec is tracking something it does not understand), must
 * report the peer as unmuted, and must already be glowing — twice over, so a
 * transient caught on its way out cannot be mistaken for a settled glow.
 */
async function settledGlowingTile(hostPage: Page): Promise<{ tileId: string; tile: Locator }> {
  const tiles = hostPage.locator("#grid-container .grid-item");
  await expect(tiles.first()).toBeVisible({ timeout: 30_000 });
  await expect(tiles, "expected exactly one remote tile (the guest) in the grid").toHaveCount(1);
  const tile = tiles.first();

  const tileId = await tile.getAttribute("id");
  expect(tileId, "the guest tile needs a stable id to track across renders").toBeTruthy();

  await expect(tile.locator("[data-mic-muted]").first()).toHaveAttribute(
    "data-mic-muted",
    "false",
    { timeout: 30_000 },
  );

  await expect
    .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
      timeout: 45_000,
      message: "expected the guest tile to enter the speaking-glow state",
    })
    .toBe("lit");
  await hostPage.waitForTimeout(1_000);
  await expect
    .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
      timeout: 10_000,
      message: "expected a settled glow on the tile before recording",
    })
    .toBe("lit");

  return { tileId: tileId as string, tile };
}

/**
 * Start recording the tile's rendered glow and the DOM writes that drive it.
 *
 * Two capture paths, as in `speaking-glow-mute-veto.spec.ts`. The
 * MutationObserver counts every `style` write Dioxus performs on the tracked
 * tile (the level-update denominator) and records each write's `oldValue`, so a
 * glow switched off and back on inside one microtask batch is still preserved.
 * The 100 ms interval re-queries the id from scratch, so the recording keeps
 * reporting if Dioxus rebuilds the tile outside the observed subtree, and it
 * records the "tile is missing" case that would make a clean result vacuous.
 *
 * THROWS if `#grid-container` is absent rather than degrading to interval-only
 * sampling: the denominator would silently become zero and every ratio built on
 * it would be meaningless.
 *
 * Returns the in-page `Date.now()` at which recording began, so the arms
 * recorded by the timer probe — which has been running since page load — can be
 * sliced to this window.
 */
async function startChurnObservation(page: Page, tileId: string): Promise<number> {
  return page.evaluate((id) => {
    const w = window as unknown as ChurnWindow;
    const samples: GlowSample[] = [];
    const recorder: ChurnRecorder = { samples, styleWrites: 0, styleTrack: [], stop: () => {} };
    w.__vcGlowChurn = recorder;

    const sample = () => {
      const el = document.getElementById(id);
      samples.push({
        at: Date.now(),
        style: el?.getAttribute("style") || "",
        missing: el === null,
      });
    };

    sample();

    const container = document.getElementById("grid-container");
    if (!container) {
      throw new Error(
        "#grid-container not found — cannot install the style MutationObserver, so the " +
          "level-update denominator would be zero and the throttle assertion vacuous",
      );
    }

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.attributeName === "style" && (record.target as Element).id === id) {
          recorder.styleWrites += 1;
          recorder.styleTrack.push(record.oldValue || "");
          // The value the tile held BEFORE this mutation. Re-reading the DOM in
          // the callback (as `sample()` does) sees only the value at the end of
          // the batch, so a glow driven dark and re-lit inside one microtask
          // batch would leave no trace — exactly the shape a wedged re-arm
          // produces, since the next PCM event re-lights the tile immediately.
          samples.push({ at: Date.now(), style: record.oldValue || "", missing: false });
        }
      }
      sample();
    });
    observer.observe(container, {
      subtree: true,
      attributes: true,
      attributeOldValue: true,
      attributeFilter: ["style"],
    });

    // `setInterval`, not `setTimeout`: the probe counts `setTimeout` calls, and
    // the recorder must not appear in its own measurement.
    const timer = window.setInterval(sample, 100);
    recorder.stop = () => {
      observer.disconnect();
      window.clearInterval(timer);
      // Closes the `oldValue` chain, or the LAST rendered style is missing. No
      // `takeRecords` flush: callbacks are microtasks, this is a fresh task.
      const el = document.getElementById(id);
      if (el !== null) {
        recorder.styleTrack.push(el.getAttribute("style") || "");
      }
    };

    return Date.now();
  }, tileId);
}

interface ChurnResult {
  at: number;
  samples: GlowSample[];
  styleWrites: number;
  styleTrack: string[] | null;
  probeInstalled: boolean;
  totalTimers: number;
  arms: number[];
}

async function stopChurnObservation(page: Page): Promise<ChurnResult> {
  return page.evaluate(() => {
    const w = window as unknown as ChurnWindow;
    w.__vcGlowChurn?.stop();
    const probe = w.__vcTimerProbe;
    return {
      at: Date.now(),
      samples: w.__vcGlowChurn?.samples ?? [],
      styleWrites: w.__vcGlowChurn?.styleWrites ?? 0,
      styleTrack: w.__vcGlowChurn ? w.__vcGlowChurn.styleTrack : null,
      probeInstalled: probe !== undefined,
      totalTimers: probe?.total ?? 0,
      arms: probe?.deadmanArms ?? [],
    };
  });
}

function parseOuterBlurPx(style: string): number | null {
  const match = /box-shadow:\s*0\s+0\s+(\d+(?:\.\d+)?)px/.exec(style);
  return match === null ? null : Number.parseFloat(match[1]);
}

function renderedBlurSteps(blurs: number[]): number[] {
  const steps: number[] = [];
  for (let i = 1; i < blurs.length; i += 1) {
    const step = Math.abs(blurs[i] - blurs[i - 1]);
    if (step > 0) {
      steps.push(step);
    }
  }
  return steps;
}

/** Smallest interval between consecutive arms, for the failure message. */
function minGapMs(arms: number[]): number | null {
  if (arms.length < 2) {
    return null;
  }
  return arms.slice(1).reduce((min, at, i) => Math.min(min, at - arms[i]), Number.MAX_SAFE_INTEGER);
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

test.describe("Speaking-glow deadman re-arm throttle", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a continuously speaking peer rebuilds the glow deadman at most once per throttle period", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_churn_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: [...BROWSER_ARGS] });
    const guestBrowser = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`],
    });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "glowchurnhost@videocall.rs",
        "GlowChurnHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "glowchurnguest@videocall.rs",
        "GlowChurnGuest",
        uiURL,
      );

      // Install the timer probe on the HOST context BEFORE its first page
      // exists, so it is in place before the wasm boots and can never miss an
      // arm. Only the host runs a `PeerTile` for the speaking guest; the local
      // self-view is the `Host` component, which drives its glow through a
      // `use_effect` and owns no deadman.
      await hostCtx.addInitScript((deadmanMs: number) => {
        const w = window as unknown as ChurnWindow;
        if (w.__vcTimerProbe) {
          return;
        }
        const probe: TimerProbe = { total: 0, deadmanArms: [] };
        w.__vcTimerProbe = probe;

        // Bound to `window` up front: the wasm-bindgen glue calls the FREE
        // identifier `setTimeout(...)`, so the wrapper is invoked with an
        // undefined receiver and must not forward `this`. The cast also pins
        // the DOM signature — `@types/node` contributes a `setTimeout` overload
        // returning `Timeout`, which is not what runs in a browser page.
        const original = window.setTimeout.bind(window) as unknown as (
          handler: TimerHandler,
          timeout?: number,
          ...rest: unknown[]
        ) => number;
        const patched = function (
          handler: TimerHandler,
          timeout?: number,
          ...rest: unknown[]
        ): number {
          probe.total += 1;
          if (timeout === deadmanMs) {
            probe.deadmanArms.push(Date.now());
          }
          return original(handler, timeout, ...rest);
        };
        window.setTimeout = patched as typeof window.setTimeout;
      }, GLOW_DEADMAN_MS);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await enterTwoUserMeeting(hostPage, guestPage, meetingId);
      await enableMic(guestPage);

      const { tileId, tile } = await settledGlowingTile(hostPage);
      const startedAt = await startChurnObservation(hostPage, tileId);
      await hostPage.waitForTimeout(OBSERVE_MS);
      const result = await stopChurnObservation(hostPage);

      // --- The probe itself must be alive and reaching the wasm timer --------
      expect(
        result.probeInstalled,
        "the setTimeout probe was never installed on the host page",
      ).toBe(true);
      expect(
        result.totalTimers,
        "the patched setTimeout recorded no calls at all — the wrapper is not intercepting, " +
          "so every count below would be a zero that proves nothing",
      ).toBeGreaterThan(0);

      const arms = result.arms.filter((at) => at >= startedAt && at <= result.at);
      expect(
        arms.length,
        `no ${GLOW_DEADMAN_MS}ms timer was observed in the window (${result.totalTimers} timers ` +
          `of other durations were). Either the deadman no longer runs through a global ` +
          `setTimeout, or GLOW_DEADMAN_MS has drifted from ${GLOW_DEADMAN_MS} — in both cases ` +
          `the throttle assertion below would pass on an empty set`,
      ).toBeGreaterThanOrEqual(1);

      // --- The recording must describe a live, glowing tile -----------------
      const samples = result.samples;
      expect(
        samples.length,
        "too few samples — the recorder was not running for the observation window",
      ).toBeGreaterThan(100);
      expect(
        samples.filter((s) => s.missing).length,
        "the tracked tile disappeared mid-window — the recording is not meaningful",
      ).toBe(0);
      const unknown = samples.filter((s) => classifyGlow(s.style) === "unknown");
      expect(
        unknown.length,
        `${unknown.length} sample(s) carried a style this spec cannot classify as speak_style ` +
          `output — first: "${unknown[0]?.style}"`,
      ).toBe(0);

      // --- The safety half: the throttle must not become a mute button ------
      const silent = samples.filter((s) => classifyGlow(s.style) === "silent");
      expect(
        silent.length,
        `the glow went dark in ${silent.length}/${samples.length} samples while the guest was ` +
          `still speaking. The window is ${OBSERVE_MS}ms — longer than the ${GLOW_DEADMAN_MS}ms ` +
          `deadman — so the timer pending at the start fired without being replaced: the re-arm ` +
          `was suppressed when it should have been admitted (issue 2225 throttle wedging the ` +
          `issue 2174 deadman). First dark sample at +${
            silent[0] ? silent[0].at - startedAt : 0
          }ms`,
      ).toBe(0);

      // --- Non-vacuity: level updates really were arriving -------------------
      expect(
        result.styleWrites,
        `only ${result.styleWrites} glow style write(s) in ${OBSERVE_MS}ms — the guest was not ` +
          `driving level updates (dead fake mic, stalled audio path), so a low re-arm count ` +
          `would prove nothing about the throttle`,
      ).toBeGreaterThanOrEqual(MIN_STYLE_WRITES);

      // --- The regression assertion -----------------------------------------
      const gap = minGapMs(arms);
      expect(
        arms.length,
        `the glow deadman was rebuilt ${arms.length} times in ${OBSERVE_MS}ms (budget ` +
          `${ARM_BUDGET} = one per ${GLOW_DEADMAN_REARM_THROTTLE_MS}ms period + 2 for the ` +
          `boundaries; smallest observed gap ${gap === null ? "n/a" : `${gap}ms`}). Issue 2225: ` +
          `arm_glow_deadman must reuse a pending timer that is younger than the throttle instead ` +
          `of allocating a Closure + setTimeout per speech event — the un-fixed build arms once ` +
          `per event, and this peer produced ${result.styleWrites} level updates in the window`,
      ).toBeLessThanOrEqual(ARM_BUDGET);

      expect(
        result.styleWrites,
        `${result.styleWrites} level updates produced ${arms.length} deadman rebuilds — barely ` +
          `throttled at all. A build that re-arms per event would show roughly one rebuild per ` +
          `update. The same verdict as the budget above but as a ratio, so it assumes nothing ` +
          `about the event rate a machine achieves: the signal is only ever fed values the ` +
          `decoder emitted (peer_tile.rs::apply_resolved_level), and 2289's UI gate (0.04) is ` +
          `WIDER than the codec-side AUDIO_LEVEL_DELTA_THRESHOLD (0.02), so writes are a subset ` +
          `of qualifying events and the un-fixed build has arms >= styleWrites. Margin note: a ` +
          `correct build is PREDICTED (never yet measured) at ~71 writes, against a ` +
          `3 * ARM_BUDGET ceiling of ${3 * ARM_BUDGET}`,
      ).toBeGreaterThanOrEqual(3 * arms.length);

      expect(
        classifyGlow((await tile.getAttribute("style")) || ""),
        "final state, read straight from the DOM rather than the recording",
      ).toBe("lit");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});

function stepHistogram(steps: number[]): string {
  const counts = new Map<number, number>();
  for (const step of steps) {
    counts.set(step, (counts.get(step) ?? 0) + 1);
  }
  return `{${[...counts.entries()]
    .sort((a, b) => a[0] - b[0])
    .map(([px, n]) => `${px}px: ${n}`)
    .join(", ")}}`;
}

test.describe("Tile audio-level write gate", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a continuously speaking peer never re-renders its tile on a sub-threshold level move", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_gate_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: [...BROWSER_ARGS] });
    const guestBrowser = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`],
    });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "glowgatehost@videocall.rs",
        "GlowGateHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "glowgateguest@videocall.rs",
        "GlowGateGuest",
        uiURL,
      );

      // HOST context, before its first page: appearance is the VIEWER's local
      // preference, and the host is the viewer that renders the guest's `PeerTile`.
      await hostCtx.addInitScript(APPEARANCE_SEED_INIT_SCRIPT);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // NODE-side: an in-page array would be wiped by a reload. Quoted into the
      // messages rather than asserted on, so an unrelated warning cannot turn this red.
      const hostPageErrors: string[] = [];
      hostPage.on("pageerror", (err) => hostPageErrors.push(err.message));

      // No camera seed, deliberately: the glow path is audio-only, `.grid-item`
      // exists for a camera-off peer, and `enterTwoUserMeeting` settles on
      // `.canvas-container` — a div. That is why issue 2193 does not block this.
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);
      await enableMic(guestPage);

      const { tileId, tile } = await settledGlowingTile(hostPage);
      await startChurnObservation(hostPage, tileId);
      await hostPage.waitForTimeout(OBSERVE_MS);
      const result = await stopChurnObservation(hostPage);

      const errNote = hostPageErrors.length
        ? ` (host page error during the window: "${hostPageErrors[0]}")`
        : "";

      // === SANITY FLOOR — presence before measurement ======================
      expect(
        result.styleTrack,
        `the ordered style track is null: the recorder never installed on the host page, so ` +
          `"no undersized steps" would be a statement about an empty list${errNote}`,
      ).not.toBeNull();
      const track = result.styleTrack as string[];

      expect(
        result.samples.filter((s) => s.missing).length,
        `the tracked tile disappeared mid-window — the recording is not meaningful${errNote}`,
      ).toBe(0);

      expect(
        track.length,
        `only ${track.length} rendered style(s) in ${OBSERVE_MS}ms. The guest was not driving ` +
          `level updates (dead fake mic, stalled audio path), so a clean step histogram would ` +
          `prove nothing${errNote}`,
      ).toBeGreaterThanOrEqual(MIN_RENDERED_STYLES);

      const misclassified = track.filter((s) => classifyGlow(s) !== "lit");
      expect(
        misclassified.length,
        `${misclassified.length}/${track.length} rendered styles were not speak_style's GLOWING ` +
          `branch — first: "${misclassified[0]}". The guest speaks continuously for the whole ` +
          `window, so a dark or unclassifiable tile means this run measured something else`,
      ).toBe(0);

      const parsed = track.map(parseOuterBlurPx);
      const unparsed = parsed.filter((b) => b === null).length;
      expect(
        unparsed,
        `${unparsed}/${track.length} rendered styles carried no outer box-shadow. parseOuterBlurPx ` +
          `is anchored on speak_style's glowing format string (canvas_generator.rs ` +
          `"box-shadow: 0 0 {:.0}px {:.0}px rgba(...)"), so it reads the outer blur and skips the ` +
          `inset shadow that follows, and yields null rather than a 0 that would silently join the ` +
          `measurement as a real reading. speak_style emits "box-shadow: none" when ` +
          `inner_glow_strength <= f32::EPSILON, and in that branch border_alpha clamps to 1.00 at ` +
          `EVERY level — the style string would be constant and every step assertion vacuous. The ` +
          `appearance seed did not take${errNote}`,
      ).toBe(0);
      const blurs = parsed as number[];

      const peak = Math.max(...blurs);
      expect(
        peak,
        `peak rendered blur was ${peak}px. At the seeded sliders the formula is ` +
          `14 + ${BLUR_PX_PER_LEVEL.toFixed(1)} * level, and this fixture saturates level at ` +
          `1.0, so a correct run peaks near 67px. A lower peak means the appearance seed did ` +
          `not take: a weaker inner_glow_strength shrinks BLUR_PX_PER_LEVEL and the step floor ` +
          `with it, so the glow geometry is not the one MIN_RENDERED_BLUR_STEP_PX ` +
          `(${MIN_RENDERED_BLUR_STEP_PX}px) was derived from${errNote}`,
      ).toBeGreaterThanOrEqual(MIN_PEAK_BLUR_PX);

      const steps = renderedBlurSteps(blurs);
      expect(
        steps.length,
        `${steps.length} non-zero blur step(s) across ${track.length} rendered styles: the tile ` +
          `re-rendered without the level moving, so there is nothing to measure${errNote}`,
      ).toBeGreaterThanOrEqual(MIN_NONZERO_STEPS);

      // === THE DISCRIMINATOR ===============================================
      const undersized = steps.filter((s) => s < MIN_RENDERED_BLUR_STEP_PX);
      expect(
        undersized.length,
        `${undersized.length} consecutive rendered blur values differed by less than ` +
          `${MIN_RENDERED_BLUR_STEP_PX}px (histogram ${stepHistogram(steps)} over ` +
          `${result.styleWrites} style writes). Issue 2289: a tile write requires ` +
          `|Δlevel| > UI_AUDIO_LEVEL_DELTA (${UI_AUDIO_LEVEL_DELTA}), which is ` +
          `${(UI_AUDIO_LEVEL_DELTA * BLUR_PX_PER_LEVEL).toFixed(3)}px of blur before rounding — ` +
          `and rounding moves each endpoint by at most 0.5px, so the rounded difference still ` +
          `exceeds 1.128 and is therefore >= 2. A 1px step is arithmetically impossible, which ` +
          `is what makes this assertion machine- and rate-independent. Zero steps (a re-render ` +
          `that wrote an unchanged blur) are dropped from the histogram, which cannot hide a 1px ` +
          `step. Seeing them means the gate is back at or below the producer's own 0.02 ` +
          `(neteq_audio_decoder.rs:194 VadState::observe, re-anchored at :200 on every emit), ` +
          `where every emitted event reaches the signal and re-renders the whole PeerTile.\n` +
          `${RENDER_CHAIN}${errNote}`,
      ).toBeLessThanOrEqual(ONE_PX_STEP_BUDGET);

      // === CORROBORATING — agrees with the verdict, does not set it =========
      expect(
        result.styleWrites,
        `${result.styleWrites} tile style writes in ${OBSERVE_MS}ms, over a ceiling of ` +
          `${MAX_STYLE_WRITES}. Each write costs more than ${UI_AUDIO_LEVEL_DELTA} of level ` +
          `movement and this fixture supplies only ${FIXTURE_LEVEL_TRAVEL_PER_ENVELOPE.toFixed(3)} ` +
          `per ${FIXTURE_ENVELOPE_MS}ms envelope (audio-fixtures.ts: ENVELOPE_PERIOD_SECONDS = 2, ` +
          `amplitude 0.6 -> 0.085 -> 0.6, which rms_to_intensity maps to level 1.0 -> 0.708), ` +
          `so a correct build cannot reach it; the un-fixed gate is predicted at ~141. Read it as ` +
          `corroboration of the step histogram above, not as the verdict — it leans on the ` +
          `fixture's travel, which the capture path can perturb${errNote}`,
      ).toBeLessThanOrEqual(MAX_STYLE_WRITES);

      expect(
        classifyGlow((await tile.getAttribute("style")) || ""),
        `final state, read straight from the DOM rather than the recording${errNote}`,
      ).toBe("lit");
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
