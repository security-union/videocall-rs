import { test, expect, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { CQI } from "../helpers/rust-mirrored-constants";
import {
  deepestTrendY,
  newestTrendY,
  SELF_SIGNAL_DISC,
  SELF_SIGNAL_SPARK,
  SPARK_MIN_POINTS,
  SPARK_REMOVED_CIRCLES,
  SPARK_TREND,
  SPARK_WARN_Y,
  SPARK_X_NEWEST,
} from "../helpers/signal-meter";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Self signal disc: Good -> Warn -> Critical -> Good through the production
 * hysteresis path (#367, #2661). Issue 2661 made the meter always-mounted, so
 * the old join gate counted a class that now exists nowhere and passed VACUOUSLY.
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
 * (dioxus-ui/src/components/connection_quality_inject.rs); everything
 * downstream of the bus is untouched production code.
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
 * pipeline reaches each rendered state and comes back.
 */

const GAP_RESET_DEADLINE_MS = 2_000;
const FASTEST_COUNTER_EXIT_MS = CQI.EXIT_COUNT * 1_000;
if (GAP_RESET_DEADLINE_MS >= FASTEST_COUNTER_EXIT_MS) {
  throw new Error(
    `GAP_RESET_DEADLINE_MS (${GAP_RESET_DEADLINE_MS}) must stay below the fastest ` +
      `counter-driven exit (${FASTEST_COUNTER_EXIT_MS}ms) or the gap-reset test stops ` +
      `discriminating: both exit paths would fit inside the deadline.`,
  );
}

const WARN_RTT = CQI.WARN_THRESHOLD_MS + 50; // 350: in [WARN, CRITICAL)
const CRITICAL_RTT = CQI.CRITICAL_THRESHOLD_MS + 150; // 650: >= CRITICAL
const GOOD_RTT = 40; // well under WARN

// GOOD IS 4, NOT 5: self moved off `Excellent`.
const LEVEL = { GOOD: "4", WARN: "3", CRITICAL: "1" } as const;

const TREND = {
  GOOD: "rgb(76, 175, 80)",
  WARN: "rgb(255, 193, 7)",
  CRITICAL: "rgb(255, 68, 68)",
} as const;

const ARIA = {
  GOOD: "Your connection: good. Open diagnostics.",
  WARN: "Your connection: slow. Open diagnostics.",
  CRITICAL: "Your connection: poor. Open diagnostics.",
} as const;

// `toContainText`: `action_bar_announce_text` appends U+00A0 on odd nonces (1765).
const ANNOUNCE = {
  CRITICAL: "Your connection is poor.",
  RECOVERED: "Your connection is back to normal.",
} as const;

const HOOK = "__videocall_inject_server_rtt";

type InjectHookWindow = Window & {
  __videocall_inject_server_rtt?: (rttMs: number, count?: number, tsMs?: number) => boolean;
};

// The live region is a SIBLING of the disc; scoping it off the disc hangs.
const chrome = (page: Page): Locator => page.locator(".host-tile-chrome");
const disc = (page: Page): Locator => chrome(page).locator(SELF_SIGNAL_DISC);
const liveRegion = (page: Page): Locator =>
  chrome(page).locator('span[role="status"][aria-live="polite"]');
const spark = (page: Page): Locator => disc(page).locator(SELF_SIGNAL_SPARK);
const sparkRuns = (page: Page): Locator => spark(page).locator(SPARK_TREND);

// WINDOWED, not newest-only: the real 1 Hz tick retakes the newest slot within a
// second of an injection, so polling the newest y here would be a race.
async function trendDepthBelow(page: Page, floor: number, message: string): Promise<number> {
  await expect
    .poll(async () => await deepestTrendY(spark(page)), { timeout: 15_000, message })
    .toBeGreaterThan(floor);
  return await deepestTrendY(spark(page));
}

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

  // PRESENCE GATE: an absent cluster must fail here, not satisfy a count-0.
  await expect(chrome(page)).toHaveCount(1, { timeout: 15_000 });
  await expect(disc(page), "the self signal disc must be mounted").toHaveCount(1, {
    timeout: 15_000,
  });
  await expect(disc(page), "the self signal disc must be painted, not just in the DOM").toBeVisible(
    { timeout: 15_000 },
  );

  await expect(disc(page)).toHaveAttribute("data-signal-level", LEVEL.GOOD, { timeout: 15_000 });
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

async function expectDiscLevel(
  page: Page,
  level: string,
  trend: string,
  aria: string,
  word: string,
): Promise<void> {
  await expect(disc(page)).toHaveAttribute("data-signal-level", level, { timeout: 10_000 });

  await expect(disc(page)).toHaveAttribute("data-signal-state", "measured", { timeout: 10_000 });
  await expect
    .poll(async () => await sparkRuns(page).count(), { timeout: 10_000 })
    .toBeGreaterThanOrEqual(1);
  await expect(
    sparkRuns(page).first(),
    "the level colour must reach the trend's computed paint",
  ).toHaveCSS("stroke", trend);

  await expect(disc(page)).toHaveAttribute("aria-label", aria);
  await expect(disc(page)).toHaveAttribute(
    "title",
    new RegExp(`^Connection: ${word} — RTT \\d+ ms$`),
  );
  await expect(disc(page)).toHaveAttribute("data-signal-lost", "false");
}

test.describe("Self signal disc: RTT transitions (#367, #2661)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
    await enableDiagnosticsTileIndicators(context);
  });

  test("disc walks Good -> Warn -> Critical -> Good as active_server_rtt changes", async ({
    page,
  }) => {
    await joinMeeting(page, "tristate");
    await assertInjectHook(page);

    // Does NOT discriminate 2661: `spark_y` is untouched. The stroke does.
    await expect
      .poll(async () => await newestTrendY(spark(page)), {
        timeout: 15_000,
        message: "a Good link must plot its newest sample at or above the 300 ms mark",
      })
      .toBeLessThanOrEqual(SPARK_WARN_Y);

    // --- Good -> Warn ---------------------------------------------------
    // ENTER_COUNT consecutive samples in the [WARN, CRITICAL) band.
    // FAILS ON REGRESSION: if the warn threshold, the enter-counter, or the
    // mapping breaks the disc stays at LEVEL.GOOD; an inverted compare reads CRITICAL.
    await injectRtt(page, WARN_RTT, CQI.ENTER_COUNT);
    await expectDiscLevel(page, LEVEL.WARN, TREND.WARN, ARIA.WARN, "slow");

    // Warn is the state that rendered "Slow connection"; emptiness at Good proves nothing.
    await expect(disc(page)).toHaveText("");

    const warnDepth = await trendDepthBelow(
      page,
      SPARK_WARN_Y,
      `an injected ${WARN_RTT} ms sample must plot below the 300 ms mark (y > ${SPARK_WARN_Y})`,
    );

    await expect(liveRegion(page)).toHaveText("");

    // The floor is asserted, not assumed; a stutter can split the run.
    await expect
      .poll(async () => Number(await disc(page).getAttribute("data-signal-samples")), {
        timeout: 15_000,
      })
      .toBeGreaterThanOrEqual(SPARK_MIN_POINTS);
    await expect
      .poll(async () => await sparkRuns(page).count(), { timeout: 15_000 })
      .toBeGreaterThanOrEqual(1);
    await expect(
      disc(page).locator(SPARK_REMOVED_CIRCLES),
      "2661 removed the head dot AND the ring arc; neither may come back",
    ).toHaveCount(0);

    // Right-anchored at SPARK_X_LAST; re-scaling k points across the width fails.
    await expect
      .poll(
        async () => {
          const points = await sparkRuns(page).last().getAttribute("points");
          return points?.trim().split(/\s+/).pop() ?? null;
        },
        { timeout: 15_000 },
      )
      .toMatch(new RegExp(`^${SPARK_X_NEWEST.replace(".", "\\.")},\\d+\\.\\d\\d$`));

    // --- Warn -> Critical ------------------------------------------------
    // FAILS ON REGRESSION: if the critical threshold or its enter-counter
    // breaks, the disc stays amber at LEVEL.WARN and the failure names that.
    await injectRtt(page, CRITICAL_RTT, CQI.ENTER_COUNT);
    await expectDiscLevel(page, LEVEL.CRITICAL, TREND.CRITICAL, ARIA.CRITICAL, "poor");
    await expect(disc(page)).toHaveText("");

    await trendDepthBelow(
      page,
      warnDepth,
      `an injected ${CRITICAL_RTT} ms sample must plot below the deepest Warn sample ` +
        `(y > ${warnDepth}) — depth, not hue, is what a colour-blind reader gets`,
    );

    await expect(liveRegion(page)).toContainText(ANNOUNCE.CRITICAL, { timeout: 10_000 });

    // --- Critical -> Good ------------------------------------------------
    // EXIT_COUNT consecutive samples below WARN. The component skips straight
    // from Critical to Good when `below_warn_count` reaches EXIT_COUNT (it does
    await injectRtt(page, GOOD_RTT, CQI.EXIT_COUNT);
    await expectDiscLevel(page, LEVEL.GOOD, TREND.GOOD, ARIA.GOOD, "good");

    await expect
      .poll(async () => await newestTrendY(spark(page)), {
        timeout: 15_000,
        message: "recovery must lift the newest sample back to or above the 300 ms mark",
      })
      .toBeLessThanOrEqual(SPARK_WARN_Y);

    // HEADLINE: Good no longer unmounts the meter. Restore the early return and
    // this goes to count 0.
    await expect(disc(page), "the disc must survive a return to Good").toHaveCount(1);
    await expect(disc(page)).toBeVisible();

    // A direct Critical -> Good step, so this does NOT discriminate latch-vs-edge.
    await expect(liveRegion(page)).toContainText(ANNOUNCE.RECOVERED, { timeout: 10_000 });
  });

  test("the disc is a button that is never itself a live region", async ({ page }) => {
    // `role="status"` on a named `<button>` makes several AT read the name and
    // suppress the content. FAILS ON REGRESSION both ways.
    await joinMeeting(page, "liveregion");

    await expect(disc(page)).toHaveJSProperty("tagName", "BUTTON");
    await expect(disc(page)).not.toHaveAttribute("role", "status");
    await expect(disc(page)).not.toHaveAttribute("aria-live", "polite");

    await expect(liveRegion(page)).toHaveCount(1);
    await expect(liveRegion(page)).toHaveAttribute("aria-atomic", "true");
    await expect(liveRegion(page)).toHaveText("");
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
    //          critical sample and the disc turns red.
    // UNFIXED: C is processed, zeroing `above_critical_count` and rewinding the
    //          watermark to t-R. D then reads as an (R+20) ms gap — still under
    //          SAMPLE_GAP_RESET_MS, so not even a reset — and leaves
    //          `above_critical_count` at 1; the disc stays green and times out.
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

    await expectDiscLevel(page, LEVEL.CRITICAL, TREND.CRITICAL, ARIA.CRITICAL, "poor");
  });

  test("a sample gap longer than the reset window clears an active warning", async ({ page }) => {
    // THE DEADLINE IS THE DISCRIMINATOR, NOT THE SAMPLE VALUE: the 1 Hz tick
    // retires a stuck ring eventually, so a long-timeout assertion passes on
    // fixed AND broken code. FAILS ON REGRESSION: neuter `SampleAction::Reset`.
    await joinMeeting(page, "gapreset");
    await assertInjectHook(page);

    await injectRtt(page, CRITICAL_RTT, CQI.ENTER_COUNT);
    await expectDiscLevel(page, LEVEL.CRITICAL, TREND.CRITICAL, ARIA.CRITICAL, "poor");

    const accepted = await page.evaluate(
      ({ criticalRtt, gapMs }) => {
        const fn = (window as InjectHookWindow).__videocall_inject_server_rtt;
        return typeof fn === "function" ? fn(criticalRtt, 1, Date.now() + gapMs + 1_000) : false;
      },
      { criticalRtt: CRITICAL_RTT, gapMs: CQI.SAMPLE_GAP_RESET_MS },
    );
    expect(accepted, "the gap sample was rejected by the hook").toBe(true);

    // "NOT Critical" rather than a positive level: `fold_sample` clears history
    // too, so the disc reads Measuring for a tick. The counter path fails both.
    await expect
      .poll(async () => await disc(page).getAttribute("data-signal-level"), {
        timeout: GAP_RESET_DEADLINE_MS,
        message:
          `the disc must leave Critical within ${GAP_RESET_DEADLINE_MS}ms of the gap sample — ` +
          `only SampleAction::Reset does that this fast. The counter-driven exit needs ` +
          `>= ${FASTEST_COUNTER_EXIT_MS}ms, so a still-red disc here means the reset arm ` +
          `did not fire.`,
      })
      .not.toBe(LEVEL.CRITICAL);

    await expect(disc(page)).toHaveAttribute("data-signal-level", LEVEL.GOOD, { timeout: 15_000 });
  });
});
