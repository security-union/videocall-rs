import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { MAX_KEYFRAME_LESS_HOLD_MS, MAX_PLAYOUT_AGE_MS } from "../helpers/rust-mirrored-constants";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for the #1020 jitter-buffer freshness deadline (issue #1022).
 *
 * ## What this guards
 *
 * The freshness deadline (#1020) drops a stale head-of-line VIDEO backlog and
 * either skips to a buffered keyframe or — when none is buffered — evicts the
 * stale deltas and holds the last-good frame while a fresh keyframe is fetched.
 * It runs INSIDE the decoder Web Worker's `JitterBuffer`, on a ~10ms tick
 * (`videocall-codecs/src/bin/worker_decoder.rs`), so its outcome used to be
 * invisible to a browser test for two reasons:
 *
 *   1. there was no way to deterministically force a *stale* backlog into the
 *      worker's buffer from the page, and
 *   2. the skip never crossed the worker→main boundary.
 *
 * Issue #1045 fixed (2): the worker posts a `FreshnessSkipMessage` that the main
 * thread re-broadcasts as a `freshness_skip` `DiagEvent` (subsystem `video`,
 * `head_age_ms`, `keyframe_seq` = -1 for the keyframe-less held case, `dropped`).
 * Issue #1022 (this spec + its injection hook) fixes (1) and asserts the event.
 *
 * ## How the test drives it
 *
 * Two `MOCK_PEERS_ENABLED`-gated `window` hooks (see
 * `videocall_client::freshness_inject`, registered by
 * `dioxus-ui/src/components/freshness_inject.rs`):
 *
 *   - `__videocall_inject_stale_video_backlog(numFrames, ageMs)` builds a
 *     self-contained test decoder (its own worker, running the PRODUCTION
 *     `worker_decoder` binary) and injects `numFrames` delta frames whose
 *     arrival time is back-dated by `ageMs`. With no buffered keyframe the worker
 *     holds; once the back-dated head ages past `MAX_PLAYOUT_AGE_MS` (1800ms) the
 *     ~10ms tick trips the keyframe-less eviction and posts a `freshness_skip`.
 *   - `__videocall_freshness_skips` — captured `freshness_skip` events; shape in `FreshnessSkip` below.
 *
 * ## Fails if the feature regresses (genuine fail-when-broken)
 *
 * The assertion is on the `freshness_skip` EVENT, not on the injection. If the
 * deadline never fires — e.g. `enforce_freshness_deadline` stops evicting, the
 * worker stops `take_freshness_skip()`-ing, or `handle_worker_diag_message`
 * stops re-broadcasting (the #1045 surfacing path) — NO event lands in
 * `__videocall_freshness_skips`, the array stays empty, and the
 * wait-for-non-empty assertion times out and FAILS. The positive-control test
 * keys this to the deadline rather than to the injection: a FRESH backlog must
 * stay silent below the deadline AND must fire once aged past it.
 *
 * ## #1662 — keyframe-less hold-CEILING escalation (`escalated: bool`)
 *
 * Issue #1662 bounds the keyframe-less held-last-good FREEZE (the routine #1020
 * eviction bounds buffer growth, not the freeze: with no buffered keyframe there
 * is nothing live to skip to, so the last-good frame can sit frozen for minutes —
 * field-observed `head_age` reached ~28s). Once the held head ages past
 * `MAX_KEYFRAME_LESS_HOLD_MS` (6000ms) — i.e. even the publisher's slowest 5s
 * periodic-keyframe recovery has failed — the buffer signals an ESCALATION: a
 * cooldown-gated (8000ms) decoder-pipeline reset that clears stuck decode state so
 * the next keyframe decodes cleanly. The escalation is surfaced as a SEPARATE,
 * throttle-bypassed `freshness_skip` `DiagEvent` carrying `escalated: true`
 * (`videocall-codecs/src/bin/worker_decoder.rs::gate_keyframe_less_escalation`
 * force-posts it the same poll it gates one in). The collector
 * (`videocall-client/src/freshness_inject.rs::spawn_freshness_skip_collector`)
 * surfaces it as a real JS boolean `skip.escalated`.
 *
 * Two tests below pin the ceiling end-to-end — the E2E analogue of the Rust unit
 * test `keyframe_less_ceiling_triggers_gated_reset_at_ceiling_not_before`:
 *
 *   - ABOVE the ceiling (age > 6000ms) → at least one captured skip has
 *     `escalated === true` ("Test 3").
 *   - BELOW the ceiling (the existing 5000ms case) → NO captured skip is
 *     escalated; every routine eviction skip carries `escalated === false`. This
 *     is the mutation-sensitivity control: if escalation fired regardless of head
 *     age (the bug #1662 prevents), this assertion would see `escalated === true`
 *     below the ceiling and FAIL ("Test 1", extended).
 */

// Back-date the ESCALATION backlog comfortably ABOVE the ceiling so the head is
// already past MAX_KEYFRAME_LESS_HOLD_MS on the first eviction tick (head_age
// only grows from injection to the tick, never shrinks), and on a cold stream
// (no prior escalation) the worker's cooldown gate is open → it force-posts the
// `escalated: true` skip. 1000ms of margin absorbs the ~10ms tick + poll latency.
const ABOVE_CEILING_AGE_MS = MAX_KEYFRAME_LESS_HOLD_MS + 1000; // 7000ms

// Back-date the injected backlog comfortably past the freshness deadline so the
// very first ~10ms tick after injection trips it (no reliance on tight timing) —
// but deliberately BELOW the #1662 keyframe-less hold ceiling (6000ms), so this
// case exercises the routine #1020 eviction WITHOUT escalating. It is the
// below-ceiling control: every skip it produces must carry `escalated === false`.
const STALE_AGE_MS = MAX_PLAYOUT_AGE_MS + 3200; // 5000ms (< MAX_KEYFRAME_LESS_HOLD_MS)

// A "fresh" backlog age for the must-NOT-fire case: starts under the deadline, so
// nothing fires until the head has aged past it.
const FRESH_AGE_MS = 0;

// Test 2 phase 1's silence window. Must stay below MAX_PLAYOUT_AGE_MS with enough margin
// to absorb waitForTimeout overshoot.
const FRESH_SILENT_WINDOW_MS = MAX_PLAYOUT_AGE_MS / 2; // 900ms

// How many delta frames to inject. >1 so `dropped` is unambiguously >= 1 and the
// eviction has a real backlog to clear.
const INJECT_FRAMES = 5;

// For the escalation case, inject a larger backlog (mirroring the Rust unit test
// `keyframe_less_stall_buffer`, which uses ~199 deltas). The keyframe-less branch
// evicts the whole stale backlog in one `drop_frames_before` call, so a generous
// count guarantees a stale, already-past-ceiling head is present on the eviction
// tick regardless of how the worker boots — head age comes from the back-dated
// arrival time (ABOVE_CEILING_AGE_MS), not from waiting for frames to age.
const ESCALATION_INJECT_FRAMES = 100;

// Keyframe-less eviction encodes keyframe_seq as -1 (the #1045 sentinel for "no
// buffered keyframe; held last-good frame").
const KEYFRAME_LESS_SENTINEL = -1;

// Mirrors the same-named consts in videocall-client/src/freshness_inject.rs.
const INJECT_FROM_PEER = "inject-local";
const INJECT_TO_PEER = "inject-peer";
const COLD_FROM_PEER = "cold-local";
const COLD_TO_PEER = "cold-peer";

// The replay is re-stamped with the worker's own clock, so the skip trails it by MAX_PLAYOUT_AGE_MS.
const COLD_SKIP_TIMEOUT_MS = 8_000;
// Must stay under COLD_HARNESS_BOOT_REPLAY_TTL_MS (30s, freshness_inject.rs): above it a slow
// boot trips the TTL and queue_dropped fails as a replay failure.
const COLD_BOOT_TIMEOUT_MS = 15_000;

// ── Issue 1899 / discussion 1960: stream-open immediate keyframe request ──
//
// Poll ceiling for the stream-open PLI (Test 4). Deliberately BELOW MAX_PLAYOUT_AGE_MS (1800ms):
// fix (a) fires the request at INSERT time (no ~10ms tick, no throttle), so it surfaces within one
// worker→main postMessage round-trip; pre-fix, the ONLY source of a keyframe request for a
// never-decoded keyframe-less stream is the #1025 eviction path, gated on head_age >= 1800ms, which
// with age≈0 frames cannot fire until ~1800ms after injection. 1200ms sits 600ms below that floor,
// so the poll times out (FAILS) on the un-fixed code and passes comfortably on the fixed code.
const STREAM_OPEN_REQUEST_WINDOW_MS = 1200;

// Upper bound on how long after injection the stream-open request may surface, measured on the
// page's own Date.now() clock. The request is neither tick-gated (it fires inside insert_frame, not
// on the ~10ms find_and_move tick) nor throttled (a direct one-shot fire, unlike the ~1s-throttled
// record_freshness_skip) — its only latency is two postMessage hops (inject→worker, worker→main
// RequestKeyframeMessage), typically <100ms even on a loaded CI box — so 1000ms is a ~10x anti-flake
// margin while still an order of magnitude below the 1800ms a pre-fix request would have to wait for.
const STREAM_OPEN_REQUEST_MAX_LATENCY_MS = 1000;

// After the first request, a short settle to prove the one-shot: a second fresh delta must NOT
// produce a second request. Kept well below the deadline so the eviction path cannot fire either,
// though fix (a)'s #1479 arrival gate independently suppresses a second request even past it.
const ONE_SHOT_SETTLE_MS = 400;

interface FreshnessSkip {
  head_age_ms: number;
  keyframe_seq: number;
  dropped: number;
  ts_ms: number;
  // #1662 keyframe-less hold-ceiling escalation flag, surfaced by the collector
  // (videocall-client/src/freshness_inject.rs) as a real JS boolean.
  escalated: boolean;
  // #1851 wall-clock gap (ms) since the previous worker poll, surfaced by the collector
  // (videocall-client/src/freshness_inject.rs) as a real JS number (f64). Absent (undefined) on
  // the pre-#1851 build, which is what makes the Test 1 assertions below fail-when-unfixed.
  tick_gap_ms: number;
  from_peer: string;
  to_peer: string;
}

// Issue 1899 / discussion 1960: a captured proactive keyframe request, appended to
// window.__videocall_keyframe_requests by the collector
// (videocall-client/src/freshness_inject.rs::record_keyframe_request) each time the worker's jitter
// buffer fires its request_keyframe hook and the RequestKeyframeMessage reaches the main thread.
interface KeyframeRequest {
  // Backlog age (ms) the worker carried on the request. 0 for the stream-open one-shot (fix (a),
  // fired at insert time); >= MAX_PLAYOUT_AGE_MS for the #1025 freshness-deadline eviction PLI.
  head_age_ms: number;
  // Main-thread wall-clock (Date.now()) at capture, letting the spec bound how quickly the request
  // surfaced after injection.
  ts_ms: number;
}

// The injection + capture hooks are attached only when MOCK_PEERS_ENABLED=true
// (see docker-compose.e2e.yaml).
const hasInjectHook = (page: Page) =>
  page.evaluate(
    () =>
      typeof (window as unknown as { __videocall_inject_stale_video_backlog?: unknown })
        .__videocall_inject_stale_video_backlog === "function",
  );

const injectStaleBacklog = (page: Page, numFrames: number, ageMs: number) =>
  page.evaluate(
    ([n, age]) =>
      (
        window as unknown as {
          __videocall_inject_stale_video_backlog: (n: number, age: number) => void;
        }
      ).__videocall_inject_stale_video_backlog(n, age),
    [numFrames, ageMs] as const,
  );

const injectStaleBacklogCold = (page: Page, numFrames: number) =>
  page.evaluate(
    (n) =>
      (
        window as unknown as {
          __videocall_inject_stale_video_backlog_cold: (n: number) => void;
        }
      ).__videocall_inject_stale_video_backlog_cold(n),
    numFrames,
  );

interface ColdBootState {
  handshake_seen: boolean;
  queue_dropped: boolean;
}

const coldBootState = (page: Page): Promise<ColdBootState | null> =>
  page.evaluate(
    () =>
      (
        window as unknown as {
          __videocall_cold_worker_boot_state?: () => ColdBootState | null;
        }
      ).__videocall_cold_worker_boot_state?.() ?? null,
  );

const readSkips = (page: Page): Promise<FreshnessSkip[]> =>
  page.evaluate(
    () =>
      (
        (window as unknown as { __videocall_freshness_skips?: FreshnessSkip[] })
          .__videocall_freshness_skips ?? []
      ).slice() as FreshnessSkip[],
  );

const skipCount = (page: Page): Promise<number> =>
  page.evaluate(
    () =>
      (
        (window as unknown as { __videocall_freshness_skips?: unknown[] })
          .__videocall_freshness_skips ?? []
      ).length,
  );

// The bus is GLOBAL, so a poll and the assertions after it must select the same decoder's entries.
const skipsFor = (page: Page, toPeer: string): Promise<FreshnessSkip[]> =>
  page.evaluate(
    (tp) =>
      (
        (window as unknown as { __videocall_freshness_skips?: FreshnessSkip[] })
          .__videocall_freshness_skips ?? []
      ).filter((s) => s.to_peer === tp) as FreshnessSkip[],
    toPeer,
  );

// Count captured skips whose #1662 `escalated` flag is strictly === true. Read in
// the page so we observe the real JS booleans the collector wrote (not a stale
// snapshot), letting `expect.poll` wait for the throttle-bypassed escalation event.
const escalatedSkipCount = (page: Page, toPeer: string): Promise<number> =>
  page.evaluate(
    (tp) =>
      (
        (
          window as unknown as {
            __videocall_freshness_skips?: { escalated?: boolean; to_peer?: string }[];
          }
        ).__videocall_freshness_skips ?? []
      ).filter((s) => s.escalated === true && s.to_peer === tp).length,
    toPeer,
  );

// Issue 1899 / discussion 1960: read/count captured proactive keyframe requests. Evaluated in the
// page so we observe the real objects the collector wrote (not a stale snapshot), letting
// `expect.poll` wait for the stream-open request to land.
const readKeyframeRequests = (page: Page): Promise<KeyframeRequest[]> =>
  page.evaluate(
    () =>
      (
        (window as unknown as { __videocall_keyframe_requests?: KeyframeRequest[] })
          .__videocall_keyframe_requests ?? []
      ).slice() as KeyframeRequest[],
  );

const keyframeRequestCount = (page: Page): Promise<number> =>
  page.evaluate(
    () =>
      (
        (window as unknown as { __videocall_keyframe_requests?: unknown[] })
          .__videocall_keyframe_requests ?? []
      ).length,
  );

test.describe("Jitter-buffer freshness deadline (#1022 / #1020)", () => {
  // The two tests each spin up a self-contained decoder worker + WebCodecs; run
  // them serially so the (15 GiB WSL) box never has two worker pipelines plus the
  // dev stack live at once.
  test.describe.configure({ mode: "serial", timeout: 120_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `freshness_${safeLabel}_${Date.now()}`;

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const homeForm = page.locator("#meeting-id");

    // Bounce-aware join. Use the shared hydration-robust join helper
    // (e2e/helpers/join-meeting.ts, commit 21c9be7f) instead of the brittle
    // fixed-waitForTimeout + Enter + toHaveURL pattern (which flaked on the
    // History-API URL lag — the helper keys "we joined" off the home form
    // detaching, robust to that lag). On top of that, retry the WHOLE submit when
    // the SPA bounces straight back to the home page: observed in this spec's
    // SERIAL run on a contended box, the meeting route mounts (form detaches, so
    // the helper returns) but then immediately re-renders home, leaving neither
    // grid nor join-button to wait on. Treating a re-appeared `#meeting-id` form
    // as a "re-submit" signal — and the grid/join-button as the success signal —
    // de-flakes that without weakening any #1662 assertion below.
    const deadline = Date.now() + 60_000;
    let entered: "join" | "grid" | undefined;
    while (Date.now() < deadline && entered === undefined) {
      await fillAndSubmitJoinForm(page, meetingId, "freshness-user");

      const which = await waitForVisibleState(
        [
          { name: "join", locator: joinButton },
          { name: "grid", locator: grid },
          // A re-appeared home form means we bounced back to "/"; loop and
          // re-submit rather than wait out the timeout on a page that left.
          { name: "home", locator: homeForm },
        ],
        20_000,
      ).catch(() => undefined);

      if (which === "join" || which === "grid") {
        entered = which;
      }
      // which === "home" (or undefined) → fall through and re-submit.
    }

    if (entered === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect may have already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    // The attendants component (which registers the injection hook) mounts with
    // the grid.
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  // Wait until the injection/capture hook has been registered on `window`. The
  // hook is attached from a `use_hook` in the attendants component, which runs
  // shortly after the grid mounts.
  //
  // When the hook is registered it ALSO pre-warms the self-contained test decoder
  // (spawns its Web Worker). The trunk worker loader instantiates the worker's
  // wasm asynchronously, and messages posted before the worker's `main()`
  // registers `onmessage` are dropped — so after the hook appears we give the
  // worker a short settle to finish booting before the first injection. (The
  // pre-warm starts the boot at mount, so this is margin, not the full boot.)
  async function assertInjectHook(page: Page): Promise<void> {
    const deadline = Date.now() + 15_000;
    let attached = false;
    while (Date.now() < deadline) {
      if (await hasInjectHook(page)) {
        attached = true;
        // Worker-boot settle margin (see above).
        await page.waitForTimeout(1500);
        break;
      }
      await page.waitForTimeout(250);
    }
    expect(
      attached,
      "__videocall_inject_stale_video_backlog is not attached. The e2e stack must run with " +
        "MOCK_PEERS_ENABLED=true (docker/docker-compose.e2e.yaml) so the freshness inject hook " +
        "is registered (dioxus-ui freshness_inject.rs).",
    ).toBe(true);
  }

  // ──────────────────────────────────────────────────────────────────────
  // Test 1 — a STALE backlog trips the deadline → freshness_skip fires.
  // ──────────────────────────────────────────────────────────────────────
  test("@bvt1 a stale buffered-video backlog trips the freshness deadline and surfaces a skip", async ({
    page,
  }) => {
    await joinMeeting(page, "stale_fires");

    await assertInjectHook(page);

    // #1851: capture the main-thread console so we can assert the re-emitted freshness_skip
    // FIELD-LOG line carries the new `tick_gap=` token. That console.warn is the load-bearing
    // upload-pipeline delivery — videocall-codecs/src/decoder/wasm.rs warns
    // `FreshnessSkipMessage::console_line()` on every skip. Attach BEFORE injecting so the warn
    // (which fires only after the deadline trips) is never missed.
    const consoleLines: string[] = [];
    page.on("console", (msg) => {
      consoleLines.push(msg.text());
    });

    // Inject a stale keyframe-less backlog (back-dated well past the deadline).
    await injectStaleBacklog(page, INJECT_FRAMES, STALE_AGE_MS);

    // The worker tick is ~10ms; the deadline trips on the next tick after the
    // back-dated head is already older than MAX_PLAYOUT_AGE_MS (it is, by
    // STALE_AGE_MS). Poll the capture array until at least one event lands.
    await expect
      .poll(() => skipsFor(page, INJECT_TO_PEER).then((s) => s.length), {
        timeout: 30_000,
        message: "expected a freshness_skip DiagEvent after injecting a stale backlog",
      })
      .toBeGreaterThanOrEqual(1);

    const skip = (await skipsFor(page, INJECT_TO_PEER))[0];

    // Shape assertions (mirrors the #1045 event contract for the keyframe-less
    // held case):
    //   - head_age_ms must have actually tripped the deadline.
    expect(skip.head_age_ms).toBeGreaterThanOrEqual(MAX_PLAYOUT_AGE_MS);
    //   - at least one stale frame was evicted.
    expect(skip.dropped).toBeGreaterThanOrEqual(1);
    //   - no buffered keyframe to skip to → the -1 sentinel.
    expect(skip.keyframe_seq).toBe(KEYFRAME_LESS_SENTINEL);

    expect(skip.from_peer).toBe(INJECT_FROM_PEER);
    expect(skip.to_peer).toBe(INJECT_TO_PEER);

    // #1851 (a) — the collector entry now carries a numeric tick_gap_ms. On the PRE-#1851 build the
    // collector never sets this field, so `skip.tick_gap_ms` is `undefined`: `typeof` is
    // "undefined", `Number.isFinite(undefined)` is false, and `undefined >= 0` is false — so ALL
    // three assertions fail. That is the fails-on-unfixed guard for the collector surface.
    //
    // We assert only finite + `>= 0`, NOT a magnitude, and that is deliberate — traced through
    // videocall-codecs: the #1022 injection back-dates each frame's arrival, and insert_frame runs
    // an internal poll at that back-dated time (jitter_buffer.rs: insert_frame →
    // find_and_move_continuous_frames(arrival_time_ms)), stamping last_tick_time_ms to
    // `now - STALE_AGE_MS`. The next REAL eviction tick therefore observes tick_gap ≈ STALE_AGE_MS
    // (~5000ms) — a harness artifact of the back-date, not a ~10ms cadence gap and not a genuinely
    // starved tab; its exact value is scheduling-dependent, so finite + `>= 0` is the stable
    // contract. This large gap does NOT escalate the record below: escalation is gated on the
    // effective freeze-age (max(head_age, hold_duration)) >= MAX_KEYFRAME_LESS_HOLD_MS (6000ms),
    // which short-circuits BEFORE the tick_gap cooldown-bypass is consulted
    // (jitter_buffer.rs::signal_keyframe_less_ceiling). STALE_AGE_MS (5000) is below that ceiling —
    // which is exactly why the escalated===false control below still holds even though this
    // tick_gap (~5000) exceeds TICK_STARVATION_GAP_MS (2000): the bypass is cooldown-only, never
    // ceiling-lowering.
    expect(typeof skip.tick_gap_ms).toBe("number");
    expect(Number.isFinite(skip.tick_gap_ms)).toBe(true);
    expect(skip.tick_gap_ms).toBeGreaterThanOrEqual(0);

    // #1851 (b) — the re-emitted field-log console line carries the `tick_gap=` token. The exact
    // token is from FreshnessSkipMessage::console_line in videocall-codecs/src/messages.rs:
    // `[JITTER_BUFFER] freshness_skip {from}->{to}: head_age={:.0}ms tick_gap={:.0}ms dropped=...`.
    // On the PRE-#1851 build the line has NO `tick_gap=` substring, so the filtered count stays 0
    // and this poll times out and FAILS. The warn is emitted in the SAME
    // handle_worker_diag_message call that broadcasts the DiagEvent captured above, so it has
    // already fired by the time skipCount reached 1; the short poll only absorbs CDP
    // console-event delivery lag.
    await expect
      .poll(
        () =>
          consoleLines.filter(
            (l) => l.includes("[JITTER_BUFFER] freshness_skip") && l.includes("tick_gap="),
          ).length,
        {
          timeout: 5_000,
          message: "expected a re-emitted freshness_skip console line carrying the tick_gap= token",
        },
      )
      .toBeGreaterThanOrEqual(1);

    // #1662 below-ceiling CONTROL (mutation-sensitivity guard). STALE_AGE_MS
    // (5000ms) is past the #1020 freshness deadline (so a routine eviction skip
    // fires above) but BELOW MAX_KEYFRAME_LESS_HOLD_MS (6000ms), so the ceiling is
    // NOT crossed and the escalation must NOT fire. Every captured skip must
    // therefore carry escalated === false. If escalation fired regardless of head
    // age (the bug #1662 prevents), this case would surface escalated === true and
    // THIS assertion would FAIL — which is what makes Test 3's escalated === true
    // assertion meaningful rather than spurious. This is the E2E analogue of the
    // "not before" half of the unit test
    // `keyframe_less_ceiling_triggers_gated_reset_at_ceiling_not_before`.
    //
    // Give the worker well past a full ceiling's worth of ticks (> 6000ms with the
    // 5000ms head start ⇒ > 11s elapsed-since-injection would be needed to reach the
    // ceiling anyway). We wait only ~1s of additional ticks here: a 5000ms-old head
    // ages at wall-clock rate, so it cannot reach the 6000ms ceiling within this
    // window — keeping the control unambiguously below-ceiling.
    await page.waitForTimeout(1000);
    expect(await escalatedSkipCount(page, INJECT_TO_PEER)).toBe(0);
    const allBelow = await skipsFor(page, INJECT_TO_PEER);
    expect(allBelow.length).toBeGreaterThanOrEqual(1);
    for (const s of allBelow) {
      expect(s.escalated).toBe(false);
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 2 — CONTROL: a FRESH backlog must NOT fire the deadline before it ages.
  // ──────────────────────────────────────────────────────────────────────
  test("@bvt1 a fresh backlog stays silent until aged, then fires (positive control)", async ({
    page,
  }) => {
    await joinMeeting(page, "fresh_silent");

    await assertInjectHook(page);

    const before = await skipCount(page);

    await injectStaleBacklog(page, INJECT_FRAMES, FRESH_AGE_MS);

    // Phase 1: below the deadline, a correct deadline stays silent.
    await page.waitForTimeout(FRESH_SILENT_WINDOW_MS);

    const after = await skipCount(page);
    expect(after).toBe(before);

    // Phase 2: hold the same backlog and let it age.
    await expect
      .poll(() => skipCount(page), {
        timeout: 15_000,
        message:
          "positive control: this backlog must eventually fire a freshness_skip; zero means the DiagEvent path delivers nothing, so phase 1's silence proves nothing",
      })
      .toBeGreaterThan(before);
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 3 — #1662: a keyframe-less hold that crosses the CEILING escalates.
  //
  // E2E analogue of the unit test
  // `keyframe_less_ceiling_triggers_gated_reset_at_ceiling_not_before` (the
  // "at ceiling" half). Inject a stale keyframe-less backlog back-dated ABOVE
  // MAX_KEYFRAME_LESS_HOLD_MS (6000ms). The head is already past the ceiling on
  // the first eviction tick, and on a cold stream the worker's 8000ms cooldown
  // gate is open, so `gate_keyframe_less_escalation` force-posts a
  // throttle-bypassed `freshness_skip` carrying `escalated: true` (and fires the
  // decoder reset). We assert at least one captured skip has escalated === true.
  //
  // Pairs with Test 1's below-ceiling control to prove the flag tracks head age:
  // below 6000ms → no escalated skip; above 6000ms → an escalated skip.
  // ──────────────────────────────────────────────────────────────────────
  test("a keyframe-less hold past MAX_KEYFRAME_LESS_HOLD_MS escalates (escalated=true)", async ({
    page,
  }) => {
    await joinMeeting(page, "ceiling_escalates");

    await assertInjectHook(page);

    // Inject a stale keyframe-less backlog back-dated ABOVE the ceiling. The head
    // is already > MAX_KEYFRAME_LESS_HOLD_MS old, so the next eviction tick both
    // (a) records the routine keyframe-less skip (escalated=false) AND (b) consults
    // the escalation hook, which — gate open on a cold stream — force-posts the
    // escalated=true skip the same poll. A generous frame count guarantees the
    // backlog is present on the eviction tick.
    await injectStaleBacklog(page, ESCALATION_INJECT_FRAMES, ABOVE_CEILING_AGE_MS);

    // Poll for the escalated skip. The escalation is force-emitted (it bypasses the
    // buffer's ~1s record_freshness_skip throttle) on the same poll as the trigger,
    // so it lands within the worker's first deadline tick after injection; 30s is
    // generous headroom for worker-boot settle + poll latency.
    await expect
      .poll(() => escalatedSkipCount(page, INJECT_TO_PEER), {
        timeout: 30_000,
        message:
          "expected at least one freshness_skip with escalated === true after injecting a backlog past MAX_KEYFRAME_LESS_HOLD_MS (#1662)",
      })
      .toBeGreaterThanOrEqual(1);

    const skips = await readSkips(page);
    const escalated = skips.find((s) => s.escalated === true);
    expect(escalated, "an escalated skip must have been captured").toBeTruthy();

    // The escalation is the keyframe-less held case (no buffered keyframe to skip
    // to → the -1 sentinel) and its head_age must be at/above the ceiling that
    // triggered it.
    expect(escalated!.keyframe_seq).toBe(KEYFRAME_LESS_SENTINEL);
    expect(escalated!.head_age_ms).toBeGreaterThanOrEqual(MAX_KEYFRAME_LESS_HOLD_MS);
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 4 — Issue 1899 / discussion 1960: a never-decoded stream that opens
  // with a delta-only backlog fires ONE immediate keyframe request at INSERT
  // time — well before the freshness deadline the eviction path would wait for.
  //
  // The mid-GOP late-joiner shape: a receiver joining a room where a share is
  // ALREADY active starts receiving deltas with the keyframe long past. Fix (a)
  // (videocall-codecs/src/jitter_buffer.rs::insert_frame) requests a keyframe the
  // moment it can prove "frames present, no keyframe, never decoded", instead of
  // waiting the full MAX_PLAYOUT_AGE_MS (1800ms) for the #1025 keyframe-less
  // eviction path to fire its first PLI. The request travels the SAME worker→main
  // RequestKeyframeMessage path a real peer's PLI does, captured by the collector
  // (videocall-client/src/freshness_inject.rs::record_keyframe_request).
  //
  // ## Fails on the un-fixed code (discriminating timing arithmetic)
  //
  // We inject a FRESH (age≈0) delta-only backlog, so the head ages at wall-clock
  // rate from ~0. Pre-fix, NO keyframe request fires at insert time; the only
  // pre-fix source of a request for a keyframe-less stream is the #1025 eviction
  // path in enforce_freshness_deadline, gated on head_age >= MAX_PLAYOUT_AGE_MS
  // (1800ms). With age≈0 frames the EARLIEST a pre-fix request can appear is
  // ~1800ms after inject. This test polls for the request within
  // STREAM_OPEN_REQUEST_WINDOW_MS (1200ms) — 600ms below that floor — so on the
  // un-fixed code the poll observes zero requests and times out (FAIL). On the
  // fixed code the stream-open one-shot fires the request synchronously inside
  // insert_frame (no tick, no throttle), surfacing within one worker→main
  // postMessage round-trip (tens of ms), and the poll passes. Reverting fix (a)
  // breaks this test.
  //
  // Contrast with Test 2: the same fresh injection yields a different observable —
  // the request is the insert-time signal, the skip is the deadline signal.
  // ──────────────────────────────────────────────────────────────────────
  test("a fresh delta-only backlog on a never-decoded stream fires an immediate keyframe request before the freshness deadline (#1899 / disc. 1960)", async ({
    page,
  }) => {
    await joinMeeting(page, "stream_open_pli");

    await assertInjectHook(page);

    const before = await keyframeRequestCount(page);

    // Capture the inject wall-clock on the PAGE's own clock (the same Date.now() the collector
    // stamps ts_ms with) so the latency bound below is measured consistently.
    const injectMs = await page.evaluate(() => Date.now());

    // Inject a FRESH (age≈0) delta-only backlog into the never-decoded test stream. The first delta
    // triggers fix (a)'s stream-open request; the other four are suppressed by the one-shot flag +
    // the #1479 arrival gate (asserted below).
    await injectStaleBacklog(page, INJECT_FRAMES, FRESH_AGE_MS);

    // Core fails-on-unfixed assertion: a request must surface within the sub-deadline window.
    await expect
      .poll(() => keyframeRequestCount(page), {
        timeout: STREAM_OPEN_REQUEST_WINDOW_MS,
        message:
          "expected a stream-open keyframe request within 1200ms (well before the 1800ms freshness deadline)",
      })
      .toBeGreaterThan(before);

    // Exactly ONE request from the 5-frame batch: the stream-open one-shot fires on the first delta
    // and suppresses the other four (intra-batch one-shot). Only the eviction path could add more,
    // and it cannot fire this far below the deadline.
    const afterFirst = await keyframeRequestCount(page);
    expect(afterFirst).toBe(before + 1);

    const req = (await readKeyframeRequests(page))[before];
    // Stream-open shape: head_age_ms is 0 for a just-opened stream, cleanly BELOW the deadline — the
    // #1025 eviction PLI (the only pre-fix source) would instead carry head_age >= MAX_PLAYOUT_AGE_MS.
    expect(req.head_age_ms).toBeLessThan(MAX_PLAYOUT_AGE_MS);
    // Latency bound: the request surfaced well within the deadline the eviction path would wait for.
    expect(req.ts_ms - injectMs).toBeGreaterThanOrEqual(0);
    expect(req.ts_ms - injectMs).toBeLessThan(STREAM_OPEN_REQUEST_MAX_LATENCY_MS);

    // One-shot (inter-batch): a second fresh delta into the same never-decoded stream must NOT fire
    // a second request. Fix (a) sets both the stream-open one-shot flag AND the #1479 arrival gate on
    // the first fire, so a subsequent never-decoded delta cannot re-fire. Stay below the deadline so
    // the eviction path is silent too.
    await injectStaleBacklog(page, 1, FRESH_AGE_MS);
    await page.waitForTimeout(ONE_SHOT_SETTLE_MS);
    expect(await keyframeRequestCount(page)).toBe(afterFirst);
  });

  // Test 5 — issue 1741: a NEVER-PRE-WARMED worker still gets its attribution.
  test("@bvt1 a cold decoder worker is attributed despite the boot race (#1741)", async ({
    page,
  }) => {
    await joinMeeting(page, "cold_attribution");

    await injectStaleBacklogCold(page, INJECT_FRAMES);

    await expect
      .poll(() => coldBootState(page).then((s) => s?.handshake_seen ?? false), {
        timeout: COLD_BOOT_TIMEOUT_MS,
        message:
          "the cold decoder worker never handed over: this is a worker BOOT failure, not a " +
          "replay failure",
      })
      .toBe(true);

    expect(
      (await coldBootState(page))?.queue_dropped,
      "the worker's boot outran the replay queue's TTL, so the burst was dropped by design: " +
        "a boot-latency failure, not a replay failure",
    ).toBe(false);

    await expect
      .poll(() => skipsFor(page, COLD_TO_PEER).then((s) => s.length), {
        timeout: COLD_SKIP_TIMEOUT_MS,
        message:
          "the worker booted inside the TTL and the queue was not dropped, yet no ATTRIBUTED " +
          "freshness_skip appeared: the replay did not deliver the cold burst",
      })
      .toBeGreaterThan(0);

    const skip = (await skipsFor(page, COLD_TO_PEER))[0];
    expect(skip.from_peer).toBe(COLD_FROM_PEER);
    expect(skip.to_peer).toBe(COLD_TO_PEER);
    expect(skip.head_age_ms).toBeGreaterThanOrEqual(MAX_PLAYOUT_AGE_MS);
    expect(skip.keyframe_seq).toBe(KEYFRAME_LESS_SENTINEL);
  });
});
