import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Connection-quality indicator: tri-state RTT transitions (issue #367).
 *
 * The sibling spec `connection-quality.spec.ts` covers the NEGATIVE side — the
 * indicator must never appear on a healthy localhost link. This spec covers the
 * positive side: Good -> Warn -> Critical -> Good, driven end-to-end through the
 * production hysteresis path.
 *
 * ===========================================================================
 * WHAT ACTUALLY MOVES THE INDICATOR (and why it is not network emulation)
 * ===========================================================================
 *
 * `ConnectionQualityIndicator` (dioxus-ui/src/components/connection_quality_indicator.rs)
 * does not read page-load latency. It subscribes to the `videocall_diagnostics`
 * bus and consumes exactly one signal: a `DiagEvent` with
 * `subsystem == "connection_manager"`, `stream_id == None`, carrying a metric
 * named `active_server_rtt` whose value is an `f64`.
 *
 * That metric is produced ~1 Hz by `ConnectionManager::build_main_diagnostic_metrics`
 * (videocall-client/src/connection/connection_manager.rs) from
 * `ServerRttMeasurement.average_rtt` — a rolling average of APPLICATION-LEVEL
 * probes. Each probe is a `MediaType::RTT` packet built in `create_rtt_packet`,
 * sent through `Connection::send_packet_datagram`, echoed by the relay, and
 * timed in `handle_rtt_response` as `reception_time - media_packet.timestamp`.
 *
 * Because the probe rides the media transport, the levers issue #367 suggested
 * cannot reach it:
 *
 *   - CDP `Network.emulateNetworkConditions({latency})` shapes `URLLoader`
 *     resource loads. The probe is a WebSocket binary frame (WS is the default
 *     transport since issue 2045) or a WebTransport/QUIC datagram — neither is a
 *     resource load. And a single `latency` knob could not park RTT in the
 *     300-500 ms Warn band and then in the >= 500 ms Critical band while leaving
 *     the heartbeat/election traffic sharing that link undisturbed; perturbing
 *     it would trip a reconnect, whose gap-reset is the very state this spec
 *     needs to hold still.
 *   - `window.__vcNetsim` DOES reach this probe on the default transport --
 *     `WebSocketTask::send_bytes` consults `netsim_hook::shape_uplink_reliable`
 *     (`connection/websocket.rs`), and the probe arrives there via
 *     `Connection::send_packet_datagram` -> `Task::WebSocket(ws) =>
 *     ws.send_packet(packet, MediaStreamKey::Control)` (`connection/task.rs`).
 *     It is still not usable here, on two grounds. No preset lands in the
 *     300-500 ms Warn band: the ladder is `good_wifi` 20 ms,
 *     `crushed_downlink` 40 ms, `good_4g` 50 ms, `congested_wifi` 80 ms,
 *     `lossy_mobile` 150 ms, `dialup` 200 ms, `satellite` 600 ms
 *     (`videocall-netsim/src/profiles.rs`) -- it steps straight from 200 to
 *     600. And uplink shaping is indiscriminate: the hook sits in `send_bytes`
 *     beneath EVERY reliable send, and the elected connection's heartbeat rides
 *     the identical `Task::send_packet_datagram` -> `ws.send_packet(_, Control)`
 *     path (`Connection::start_heartbeat`), so an install that delays the probe
 *     delays and drops heartbeats too -- and the reconnect/re-election that
 *     provokes is the very gap-reset this spec needs to hold still. The inbound
 *     direction is separately unusable: it is documented LOSS-ONLY
 *     (`netsim_hook::shape_inbound` maps `Admission::Delay` to "deliver now"),
 *     so a `"down"` install adds no latency at all.
 *   - `helpers/downlink-impair.ts` manufactures packet LOSS (relay-side
 *     outbound-channel overflow, or client-side inbound drops). Its one latency-
 *     adjacent tool, `severWsTransport`, kills the link — producing a reconnect
 *     and a hysteresis reset, the opposite of a high-RTT steady state.
 *
 * So this spec publishes the diagnostics sample itself, via the
 * `MOCK_PEERS_ENABLED`-gated `window.__videocall_inject_server_rtt` hook
 * (dioxus-ui/src/components/connection_quality_inject.rs), and everything
 * downstream of the bus is untouched production code: the subsystem filter, the
 * metric extraction, `classify_sample`'s ordering/gap watermark,
 * `HysteresisState::update`'s counters, the level -> class/label/icon mapping,
 * and the 500 ms exit animation before the element unmounts.
 *
 * ===========================================================================
 * DETERMINISM: WHY EXACTLY `ENTER_COUNT` INJECTED SAMPLES SUFFICE
 * ===========================================================================
 *
 * The real 1 Hz `connection_manager` tick keeps emitting low-RTT samples for the
 * whole test, and a single low sample resets `above_warn_count` to 0. Injected
 * samples therefore have to arrive as an UNBROKEN run.
 *
 * They do: one `__videocall_inject_server_rtt(rtt, n)` call publishes all `n`
 * events in one synchronous wasm callback, and the real tick is a timer callback
 * — which cannot preempt a running one on the single-threaded JS event loop.
 * Ordering across the broadcast channel is emission order, so the component sees
 * the run contiguously. That is also why every multi-step sequence below lives
 * in ONE `page.evaluate` body with no `await` between the injections.
 *
 * What this determinism does NOT buy: the real stream makes the EXACT exit
 * timing unobservable (real low samples accumulate toward `EXIT_COUNT` on their
 * own). The precise counter arithmetic is pinned by the Rust unit tests in
 * `connection_quality_indicator.rs`; what this spec pins is that the whole
 * pipeline reaches each rendered state and then tears down.
 */

// --- Mirrors of dioxus-ui/src/components/connection_quality_indicator.rs ---
// These are LOCKSTEP constants, not loose test data. `ENTER_COUNT` samples is
// exactly what the tests inject, so raising the production constant without
// updating this block turns the entering assertions red — which is the intended
// signal, not flake.
const CQI = {
  WARN_THRESHOLD_MS: 300, // >= this renders "Slow connection" (2 bars)
  CRITICAL_THRESHOLD_MS: 500, // >= this renders "Poor connection" (1 bar)
  ENTER_COUNT: 3, // consecutive samples above a threshold to enter it
  EXIT_COUNT: 5, // consecutive samples below a threshold to leave it
  SAMPLE_GAP_RESET_MS: 10_000, // inter-sample gap that resets hysteresis
  EXIT_FADE_MS: 500, // gloo Timeout before the element unmounts
} as const;

// Deadline for the gap-reset test's teardown assertion. It is a DISCRIMINATOR,
// not a generous "eventually" timeout — see that test's comment for the full
// arithmetic. Derived from the two exit paths it must separate:
//   - reset path:   next render, no fade (tens of ms)
//   - counter path: EXIT_COUNT samples at the real 1 Hz tick + EXIT_FADE_MS
//                   = 5 * 1000 + 500 = 5500 ms, at the very fastest
// A retune that lowers EXIT_COUNT (or speeds the diagnostics tick) narrows this
// margin, so the floor is asserted below rather than left implicit.
const GAP_RESET_DEADLINE_MS = 2_000;
const FASTEST_COUNTER_EXIT_MS = CQI.EXIT_COUNT * 1_000 + CQI.EXIT_FADE_MS;
if (GAP_RESET_DEADLINE_MS >= FASTEST_COUNTER_EXIT_MS) {
  throw new Error(
    `GAP_RESET_DEADLINE_MS (${GAP_RESET_DEADLINE_MS}) must stay below the fastest ` +
      `counter-driven exit (${FASTEST_COUNTER_EXIT_MS}ms) or the gap-reset test stops ` +
      `discriminating: both exit paths would fit inside the deadline.`,
  );
}

// Sample values, derived from the mirrored thresholds so a retune keeps them in
// their intended bands.
const WARN_RTT = CQI.WARN_THRESHOLD_MS + 50; // 350: in [WARN, CRITICAL)
const CRITICAL_RTT = CQI.CRITICAL_THRESHOLD_MS + 150; // 650: >= CRITICAL
const GOOD_RTT = 40; // well under WARN

// The unfilled bar colour in `SignalBarsIcon` (dioxus-ui/src/components/icons/signal_bars.rs).
// Filled bars take a level-dependent colour; counting the rects that are NOT
// this colour is how the spec reads the rendered bar level out of the DOM.
const UNFILLED_BAR = "#555";

const HOOK = "__videocall_inject_server_rtt";

type InjectHookWindow = Window & {
  __videocall_inject_server_rtt?: (rttMs: number, count?: number, tsMs?: number) => boolean;
};

// --- Locators. Every selector below is anchored to production RSX: ---
// `div.host-tile-chrome` is attendants.rs (the self-view's top-right cluster);
// `ConnectionQualityIndicator {}` is its DIRECT child there, and the component's
// root element is the `div` carrying
// `class: "connection-quality-indicator visible" | "... exiting"`,
// `role: "status"`, `aria-live: "polite"`, `aria-label`, `title`. So the
// direct-child combinator below is structurally correct, not a guess.
//
// Two locators, deliberately: the component swaps `visible` for `exiting` while
// it fades, and during that fade it keeps rendering the LAST non-Good level
// (same label, same bar count). Content assertions therefore use the
// state-agnostic locator, so a real 1 Hz low sample nudging the component into
// its fade mid-assertion cannot turn a correct render into a false failure.
// Only the opacity check — the one thing that genuinely differs between the two
// classes — uses the `.visible` locator, and it runs first.
const chrome = (page: Page) => page.locator(".host-tile-chrome");
const indicatorAnyState = (page: Page) => page.locator(".connection-quality-indicator");
const indicatorVisible = (page: Page) =>
  page.locator(".host-tile-chrome > .connection-quality-indicator.visible");
const indicatorRendered = (page: Page) =>
  page.locator(".host-tile-chrome > .connection-quality-indicator");
const indicatorLabel = (page: Page) => indicatorRendered(page).locator(".connection-quality-label");
const filledBars = (page: Page) =>
  indicatorRendered(page).locator(`svg rect:not([fill="${UNFILLED_BAR}"])`);

/** Join a fresh meeting and settle on the in-call grid. */
async function joinMeeting(page: Page, label: string): Promise<void> {
  const meetingId = `e2e_cqi_${label}_${Date.now()}`;
  await fillAndSubmitJoinForm(page, meetingId, `cqi-${label}`);

  // `.first()`: `waitForVisibleState` calls `isVisible()` and swallows errors,
  // so a strict-mode violation from a multi-match locator would silently read
  // as "not visible" and skip the click.
  const joinButton = page.getByText(/Start Meeting|Join Meeting/).first();
  const grid = page.locator("#grid-container");
  const which = await waitForVisibleState(
    [
      { name: "join", locator: joinButton },
      { name: "grid", locator: grid },
    ],
    20_000,
  );
  if (which === "join" && (await joinButton.count()) > 0) {
    // Swallow click-after-detach: the auto-join effect may already have
    // transitioned past NotJoined and unmounted the button.
    await joinButton
      .first()
      .click()
      .catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });

  // PRESENCE GATE (must precede every measurement below). The indicator mounts
  // inside `div.host-tile-chrome`, which lives in the `can_stream`-gated
  // `nav#host-controls-nav`. If that cluster is absent, every later
  // "indicator is gone" assertion would pass for the wrong reason.
  await expect(chrome(page)).toHaveCount(1, { timeout: 15_000 });

  // The indicator itself renders `rsx! {}` while quality is Good, so it must be
  // absent right now — this is both the precondition for the transitions and a
  // guard that we are not reading a stale element from a previous phase.
  await expect(indicatorAnyState(page)).toHaveCount(0);
}

/**
 * Assert the MOCK_PEERS_ENABLED-gated injection hook is attached.
 *
 * MUST be called AFTER joining: the hook is registered from a `use_hook` inside
 * `AttendantsComponent`, so nothing is on `window` until the meeting view has
 * mounted. Probing it on the home page would always report "off".
 *
 * HARD-FAILS rather than skipping. `docker/docker-compose.e2e.yaml` sets
 * `MOCK_PEERS_ENABLED=true` as a literal, not a `${VAR:-}` default, so in the
 * stack this spec runs against a missing hook is NEVER an "unsupported
 * deployment" — it is a broken harness (renamed/removed hook, regressed MOCK
 * gate, or a build that stopped registering the module). Skipping would delete
 * every assertion in this file and report a false green, which is precisely the
 * failure this spec exists to catch.
 */
async function assertInjectHook(page: Page): Promise<void> {
  const attached = await page.evaluate(
    (hook) => typeof (window as InjectHookWindow)[hook as typeof HOOK] === "function",
    HOOK,
  );
  expect(
    attached,
    `${HOOK} is not attached. The e2e stack must run with MOCK_PEERS_ENABLED=true ` +
      `(docker/docker-compose.e2e.yaml) so the connection-quality inject hook is ` +
      `registered (dioxus-ui connection_quality_inject.rs). A missing hook here means ` +
      `the harness is broken, not an unsupported deployment.`,
  ).toBe(true);
}

/**
 * Publish `count` synthetic `active_server_rtt` samples in ONE synchronous
 * batch (see the determinism note in the file header). Asserts the hook
 * reported success, so a rejected argument surfaces here instead of as a
 * mystery timeout on the next expectation.
 */
async function injectRtt(page: Page, rttMs: number, count: number): Promise<void> {
  const accepted = await page.evaluate(
    ({ rtt, n }) => {
      const fn = (window as InjectHookWindow).__videocall_inject_server_rtt;
      return typeof fn === "function" ? fn(rtt, n) : false;
    },
    { rtt: rttMs, n: count },
  );
  expect(accepted, `${HOOK}(${rttMs}, ${count}) was rejected by the hook`).toBe(true);
}

test.describe("Connection quality indicator: RTT transitions (#367)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("indicator walks Good -> Warn -> Critical -> Good as active_server_rtt changes", async ({
    page,
  }) => {
    await joinMeeting(page, "tristate");
    await assertInjectHook(page);

    // --- Good -> Warn ---------------------------------------------------
    // ENTER_COUNT consecutive samples in the [WARN, CRITICAL) band.
    // FAILS ON REGRESSION: if the warn threshold, the enter-counter, or the
    // level->class mapping breaks, no `.visible` element ever appears and this
    // times out. If the CRITICAL branch were entered instead (threshold
    // comparison inverted), the label assertion below catches it.
    await injectRtt(page, WARN_RTT, CQI.ENTER_COUNT);

    // `.visible`-specific assertions run FIRST, while the fade cannot have
    // started: leaving Warn needs EXIT_COUNT real low samples (~5 s at 1 Hz)
    // and these resolve in well under a second.
    await expect(indicatorVisible(page)).toHaveCount(1, { timeout: 10_000 });
    // The pill is actually painted, not merely in the DOM: the base rule sets
    // `opacity: 0` and only `.visible` raises it to 1 (static/style.css). A
    // DOM-presence assertion alone would pass on an invisible element.
    await expect(indicatorVisible(page)).toHaveCSS("opacity", "1");

    await expect(indicatorLabel(page)).toHaveText("Slow connection");
    await expect(indicatorRendered(page)).toHaveAttribute(
      "aria-label",
      /^Connection quality: slow, round trip time \d+ milliseconds$/,
    );
    await expect(indicatorRendered(page)).toHaveAttribute("role", "status");
    await expect(indicatorRendered(page)).toHaveAttribute("aria-live", "polite");
    // Warn renders SignalBarsIcon at level 2 -> exactly two filled bars.
    await expect(filledBars(page)).toHaveCount(2);

    // --- Warn -> Critical ------------------------------------------------
    // FAILS ON REGRESSION: if the critical threshold or its enter-counter
    // breaks, the pill stays at "Slow connection"/2 bars and both assertions
    // below fail on the stale amber state — not on an absent element, so the
    // failure names the actual defect.
    await injectRtt(page, CRITICAL_RTT, CQI.ENTER_COUNT);

    await expect(indicatorVisible(page)).toHaveCount(1, { timeout: 10_000 });
    await expect(indicatorLabel(page)).toHaveText("Poor connection", { timeout: 10_000 });
    await expect(indicatorRendered(page)).toHaveAttribute(
      "aria-label",
      /^Connection quality: poor, round trip time \d+ milliseconds$/,
    );
    // Critical renders SignalBarsIcon at level 1 -> exactly one filled bar.
    await expect(filledBars(page)).toHaveCount(1);

    // --- Critical -> Good ------------------------------------------------
    // EXIT_COUNT consecutive samples below WARN. The component skips straight
    // from Critical to Good when `below_warn_count` reaches EXIT_COUNT (it does
    // not pause at Warn), then plays a EXIT_FADE_MS fade before returning
    // `rsx! {}` and unmounting.
    //
    // FAILS ON REGRESSION: the element is asserted GONE, not merely faded —
    // `.connection-quality-indicator` in ANY state (`visible` or `exiting`).
    // An exit path that leaves the pill mounted, or a fade timer that never
    // completes its `quality.set(Good)`, keeps the count at 1 and fails here.
    // (An opacity assertion could not distinguish those: the base rule is
    // already `opacity: 0`.)
    await injectRtt(page, GOOD_RTT, CQI.EXIT_COUNT);

    await expect(indicatorAnyState(page)).toHaveCount(0, {
      timeout: CQI.EXIT_FADE_MS + 5_000,
    });
  });

  test("a replayed out-of-order sample does not suppress a genuine warning", async ({ page }) => {
    // Regression guard for the #367 fix in `classify_sample`. Pre-fix, the gap
    // check computed `evt.ts_ms.saturating_sub(last_sample_ts_ms)`, which floors
    // a BACKWARDS delta to 0 — so a replayed event read as a normal contiguous
    // sample AND rewound the watermark via the unconditional
    // `last_sample_ts_ms = evt.ts_ms`.
    //
    // The sequence below is the end-to-end shape of that bug. `R` is
    // `replayOffsetMs` (5000 — half the reorder window, see below):
    //
    //   A  CRITICAL @ t        accept, watermark = t,     above_critical = 1
    //   B  CRITICAL @ t+10     accept, watermark = t+10,  above_critical = 2
    //   C  GOOD     @ t-R      a replay, R+10 ms behind the watermark
    //   D  CRITICAL @ t+20     the third consecutive critical sample
    //
    // FIXED:   C is `SampleAction::Skip` — dropped without touching the
    //          counters or the watermark — so D is the third consecutive
    //          critical sample and the "Poor connection" pill appears.
    // UNFIXED: C is processed, zeroing `above_critical_count` and rewinding the
    //          watermark to t-R. D then reads as an (R+20) ms gap — still under
    //          SAMPLE_GAP_RESET_MS, so not even a reset — and leaves
    //          `above_critical_count` at 1. The pill never appears and this
    //          test times out.
    //
    // Reverting `classify_sample` therefore breaks this test. That is only true
    // because all four samples are published in ONE synchronous evaluate body:
    // a real 1 Hz sample landing between A and D would zero the counters on the
    // fixed path too.
    await joinMeeting(page, "replay");
    await assertInjectHook(page);

    // Derived from the mirrored constant so the replay can never fall OUTSIDE
    // the reorder window: past SAMPLE_GAP_RESET_MS, `classify_sample` reads the
    // backwards jump as a clock step and RESETS instead of skipping, which
    // would test a different branch than this test names.
    const replayOffsetMs = CQI.SAMPLE_GAP_RESET_MS / 2;

    const accepted = await page.evaluate(
      ({ criticalRtt, goodRtt, backMs }) => {
        const fn = (window as InjectHookWindow).__videocall_inject_server_rtt;
        if (typeof fn !== "function") {
          return false;
        }
        // `Date.now()` here is the same clock `videocall_diagnostics::now_ms`
        // reads on wasm, so these offsets are exact against the watermark the
        // real 1 Hz samples have been advancing.
        const t = Date.now();
        return (
          fn(criticalRtt, 1, t) &&
          fn(criticalRtt, 1, t + 10) &&
          fn(goodRtt, 1, t - backMs) &&
          fn(criticalRtt, 1, t + 20)
        );
      },
      { criticalRtt: CRITICAL_RTT, goodRtt: GOOD_RTT, backMs: replayOffsetMs },
    );
    expect(accepted, "one of the four injected samples was rejected by the hook").toBe(true);

    await expect(indicatorVisible(page)).toHaveCount(1, { timeout: 10_000 });
    await expect(indicatorLabel(page)).toHaveText("Poor connection");
    await expect(filledBars(page)).toHaveCount(1);
  });

  test("a sample gap longer than the reset window clears an active warning", async ({ page }) => {
    // The other half of `classify_sample`: a FORWARD jump beyond
    // SAMPLE_GAP_RESET_MS means the connection context changed (reconnect,
    // re-election), so the stale hysteresis is wiped and a visible pill is
    // hidden at once — rather than lingering with counters describing a
    // connection that no longer exists.
    //
    // WHAT MAKES THIS DISCRIMINATING IS THE DEADLINE, NOT THE SAMPLE VALUE.
    // The gap sample is CRITICAL, but that alone does not pin anything: as this
    // file's header states, the real 1 Hz `connection_manager` tick keeps
    // emitting LOW-RTT samples throughout, so a pill left up by a broken reset
    // is eventually retired by the ordinary counter path anyway. An earlier
    // revision asserted `toHaveCount(0)` with a 10 s timeout and therefore
    // passed on both the fixed and the broken code — it could not tell the two
    // exits apart. The two paths differ in LATENCY, so the deadline is the
    // discriminator:
    //
    //   reset path   — `SampleAction::Reset` calls `hysteresis.reset()` and, if
    //                  the pill is visible, sets quality Good with `exiting`
    //                  left FALSE. The element unmounts on the next render with
    //                  no fade: tens of ms.
    //   counter path — needs EXIT_COUNT (5) consecutive accepted low samples at
    //                  the real 1 Hz tick (>= 5000 ms) and THEN the 500 ms
    //                  EXIT_FADE_MS timeout: >= 5500 ms, and materially longer
    //                  while the monotonicity guard holds, because the
    //                  future-stamped watermark makes each real sample a
    //                  backwards `Skip` until wall-clock catches up past it.
    //
    // GAP_RESET_DEADLINE_MS sits between them with ~20x margin over the reset
    // path and ~2.75x clearance under the fastest counter exit, so it is a
    // timing separation, not a race.
    //
    // FAILS ON REGRESSION: delete or neuter the `SampleAction::Reset` arm and
    // the pill survives the deadline — the counter path cannot retire it that
    // fast — so `toHaveCount(0)` fails.
    //
    // (Re-injecting CRITICAL samples to starve the counter path was considered
    // instead of the deadline. It is not clean here: one sample only defers the
    // natural exit by one tick, and after the gap sample the watermark is ~11 s
    // ahead, so any top-up stamped "now" is discarded as a backwards Skip and
    // changes nothing. Keeping them coming with future stamps for the whole
    // window would also risk re-entering Critical on the FIXED path.)
    await joinMeeting(page, "gapreset");
    await assertInjectHook(page);

    await injectRtt(page, CRITICAL_RTT, CQI.ENTER_COUNT);
    await expect(indicatorVisible(page)).toHaveCount(1, { timeout: 10_000 });
    await expect(indicatorLabel(page)).toHaveText("Poor connection");

    // One sample stamped far enough ahead to exceed the reset window. It is
    // still a CRITICAL value, so only the reset can hide the pill.
    const accepted = await page.evaluate(
      ({ criticalRtt, gapMs }) => {
        const fn = (window as InjectHookWindow).__videocall_inject_server_rtt;
        return typeof fn === "function" ? fn(criticalRtt, 1, Date.now() + gapMs + 1_000) : false;
      },
      { criticalRtt: CRITICAL_RTT, gapMs: CQI.SAMPLE_GAP_RESET_MS },
    );
    expect(accepted, "the gap sample was rejected by the hook").toBe(true);

    // The reset path calls `quality.set(Good)` with `exiting` left false, so the
    // element unmounts on the next render with no fade at all. The deadline is
    // what makes this discriminating (see the comment at the top of this test):
    // only the reset path can clear the pill this fast.
    await expect(
      indicatorAnyState(page),
      `the pill must be gone within ${GAP_RESET_DEADLINE_MS}ms of the gap sample — only ` +
        `SampleAction::Reset unmounts it that fast (no fade). The counter-driven exit ` +
        `needs >= ${FASTEST_COUNTER_EXIT_MS}ms, so a pill still mounted here means the ` +
        `reset arm did not fire.`,
    ).toHaveCount(0, { timeout: GAP_RESET_DEADLINE_MS });
  });
});
