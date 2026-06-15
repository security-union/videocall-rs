import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
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
 */

// Mirror of MAX_PLAYOUT_AGE_MS in videocall-codecs/src/jitter_buffer.rs.
// If that const is retuned, update this in lockstep.
const MAX_PLAYOUT_AGE_MS = 1800;

// Back-date the injected backlog comfortably past the deadline so the very first
// ~10ms tick after injection trips it (no reliance on tight timing).
const STALE_AGE_MS = MAX_PLAYOUT_AGE_MS + 3200; // 5000ms

// A "fresh" backlog age for the control (must-NOT-fire) case: well under the
// deadline so the head never ages past it during the observation window.
const FRESH_AGE_MS = 0;

// How many delta frames to inject. >1 so `dropped` is unambiguously >= 1 and the
// eviction has a real backlog to clear.
const INJECT_FRAMES = 5;

// Keyframe-less eviction encodes keyframe_seq as -1 (the #1045 sentinel for "no
// buffered keyframe; held last-good frame").
const KEYFRAME_LESS_SENTINEL = -1;

interface FreshnessSkip {
  head_age_ms: number;
  keyframe_seq: number;
  dropped: number;
  ts_ms: number;
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

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("freshness-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await waitForVisibleState(
      [
        { name: "join", locator: joinButton },
        { name: "grid", locator: grid },
      ],
      20_000,
    );
    if (which === "join") {
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
});
