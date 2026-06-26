import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
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
 *   - `__videocall_freshness_skips` — an array a diagnostics-bus subscriber
 *     appends each captured `freshness_skip` to (`{ head_age_ms, keyframe_seq,
 *     dropped, ts_ms }`).
 *
 * ## Fails if the feature regresses (genuine fail-when-broken)
 *
 * The assertion is on the `freshness_skip` EVENT, not on the injection. If the
 * deadline never fires — e.g. `enforce_freshness_deadline` stops evicting, the
 * worker stops `take_freshness_skip()`-ing, or `handle_worker_diag_message`
 * stops re-broadcasting (the #1045 surfacing path) — NO event lands in
 * `__videocall_freshness_skips`, the array stays empty, and the
 * wait-for-non-empty assertion times out and FAILS. Verified by a control case:
 * injecting a FRESH backlog (age well below the deadline) produces ZERO events
 * within the same window, proving the assertion is keyed to the deadline tripping
 * and not merely to the injection occurring. See the "does NOT fire" test below.
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

// Mirror of MAX_PLAYOUT_AGE_MS in videocall-codecs/src/jitter_buffer.rs.
// If that const is retuned, update this in lockstep.
const MAX_PLAYOUT_AGE_MS = 1800;

// Mirror of MAX_KEYFRAME_LESS_HOLD_MS in videocall-codecs/src/jitter_buffer.rs
// (issue #1662): the keyframe-less held-last-good freeze ceiling. Crossing it
// triggers the cooldown-gated decoder-reset escalation (`escalated: true`). If
// that const is retuned, update this in lockstep.
const MAX_KEYFRAME_LESS_HOLD_MS = 6000;

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

// A "fresh" backlog age for the control (must-NOT-fire) case: well under the
// deadline so the head never ages past it during the observation window.
const FRESH_AGE_MS = 0;

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

interface FreshnessSkip {
  head_age_ms: number;
  keyframe_seq: number;
  dropped: number;
  ts_ms: number;
  // #1662 keyframe-less hold-ceiling escalation flag, surfaced by the collector
  // (videocall-client/src/freshness_inject.rs) as a real JS boolean.
  escalated: boolean;
}

// The injection + capture hooks are attached only when MOCK_PEERS_ENABLED=true
// (see docker-compose.e2e.yaml). The whole feature this spec covers cannot run
// without them, so the tests skip (rather than fail) if they are absent.
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

// Count captured skips whose #1662 `escalated` flag is strictly === true. Read in
// the page so we observe the real JS booleans the collector wrote (not a stale
// snapshot), letting `expect.poll` wait for the throttle-bypassed escalation event.
const escalatedSkipCount = (page: Page): Promise<number> =>
  page.evaluate(
    () =>
      (
        (window as unknown as { __videocall_freshness_skips?: { escalated?: boolean }[] })
          .__videocall_freshness_skips ?? []
      ).filter((s) => s.escalated === true).length,
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
  async function waitForHook(page: Page): Promise<boolean> {
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      if (await hasInjectHook(page)) {
        // Worker-boot settle margin (see above).
        await page.waitForTimeout(1500);
        return true;
      }
      await page.waitForTimeout(250);
    }
    return false;
  }

  // ──────────────────────────────────────────────────────────────────────
  // Test 1 — a STALE backlog trips the deadline → freshness_skip fires.
  // ──────────────────────────────────────────────────────────────────────
  test("a stale buffered-video backlog trips the freshness deadline and surfaces a skip", async ({
    page,
  }) => {
    await joinMeeting(page, "stale_fires");

    if (!(await waitForHook(page))) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; freshness injection hooks are not attached");
      return;
    }

    // Inject a stale keyframe-less backlog (back-dated well past the deadline).
    await injectStaleBacklog(page, INJECT_FRAMES, STALE_AGE_MS);

    // The worker tick is ~10ms; the deadline trips on the next tick after the
    // back-dated head is already older than MAX_PLAYOUT_AGE_MS (it is, by
    // STALE_AGE_MS). Poll the capture array until at least one event lands.
    await expect
      .poll(() => skipCount(page), {
        timeout: 30_000,
        message: "expected a freshness_skip DiagEvent after injecting a stale backlog",
      })
      .toBeGreaterThanOrEqual(1);

    const skips = await readSkips(page);
    const skip = skips[0];

    // Shape assertions (mirrors the #1045 event contract for the keyframe-less
    // held case):
    //   - head_age_ms must have actually tripped the deadline.
    expect(skip.head_age_ms).toBeGreaterThanOrEqual(MAX_PLAYOUT_AGE_MS);
    //   - at least one stale frame was evicted.
    expect(skip.dropped).toBeGreaterThanOrEqual(1);
    //   - no buffered keyframe to skip to → the -1 sentinel.
    expect(skip.keyframe_seq).toBe(KEYFRAME_LESS_SENTINEL);

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
    expect(await escalatedSkipCount(page)).toBe(0);
    const allBelow = await readSkips(page);
    expect(allBelow.length).toBeGreaterThanOrEqual(1);
    for (const s of allBelow) {
      expect(s.escalated).toBe(false);
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // Test 2 — CONTROL: a FRESH backlog must NOT fire the deadline.
  //
  // This is the genuine-fail-when-broken guard. It proves Test 1's assertion is
  // keyed to the deadline TRIPPING, not merely to the injection running: the same
  // injection path with a non-stale arrival time produces ZERO events in the same
  // observation window. If the deadline fired regardless of head age (the bug
  // this whole feature prevents), THIS test would see events and fail.
  // ──────────────────────────────────────────────────────────────────────
  test("a fresh (non-stale) backlog does NOT trip the freshness deadline", async ({ page }) => {
    await joinMeeting(page, "fresh_silent");

    if (!(await waitForHook(page))) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; freshness injection hooks are not attached");
      return;
    }

    const before = await skipCount(page);

    // Inject a FRESH backlog (arrival ~= now). The head never ages past the
    // deadline during the window below, so no skip should occur.
    await injectStaleBacklog(page, INJECT_FRAMES, FRESH_AGE_MS);

    // Give the worker well over a deadline's worth of ticks. A fresh head needs
    // MAX_PLAYOUT_AGE_MS (1800ms) to even become stale; we wait less than that so
    // a correct deadline stays silent. (If we waited > 1800ms the fresh frames
    // would themselves age out and legitimately fire — that is NOT a bug, just
    // the deadline doing its job, so the window is intentionally sub-deadline.)
    await page.waitForTimeout(1200);

    const after = await skipCount(page);
    expect(after).toBe(before);
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

    if (!(await waitForHook(page))) {
      test.skip(true, "MOCK_PEERS_ENABLED is off; freshness injection hooks are not attached");
      return;
    }

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
      .poll(() => escalatedSkipCount(page), {
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
});
