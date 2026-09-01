import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { enableSimulcastFlag } from "../helpers/simulcast-config";

/**
 * Per-tile media-metrics overlay (issue 1768).
 *
 * The diagnostics drawer has a "Show media metrics on tiles" checkbox
 * (`input#diag-media-metrics-overlay`, testid `media-metrics-overlay-toggle`,
 * default OFF, persisted to localStorage key `diagnostics.media_metrics_overlay`).
 * When checked, each VIDEO tile renders a bottom-anchored, pointer-events:none
 * readout:
 *   - the local user's OWN tile → `[data-testid="media-metrics-overlay-self"]`
 *     with the SENDING resolution·fps·audio;
 *   - a REMOTE peer tile → `[data-testid="media-metrics-overlay-peer"]` with the
 *     RECEIVED resolution·fps·audio.
 * The text is always the three-segment `"{res} · {fps}fps · {audio}k"` shape (an
 * absent metric renders an em-dash), so every overlay contains `"fps"`.
 *
 * Issue #1784: the REMOTE peer's fps is the PAINTED rate (frames drawn to the
 * canvas), sourced from the decoder's per-peer `video_painted` diagnostics event —
 * not the arrival-rate `fps_received` bucket, which still feeds the diagnostics
 * drawer / signal popup unchanged. The visible three-segment format is unchanged;
 * the painted-vs-arrival source is guarded at the unit level (`overlay_painted_fps_
 * sample`, `PaintRateMeter` in videocall-client), while this spec asserts the
 * painted route reaches the overlay as a real number.
 *
 * Selectors below are anchored to the RSX authored in this change:
 *   - checkbox / label:  dioxus-ui/src/components/diagnostics.rs (Display options)
 *   - overlay element:   dioxus-ui/src/components/media_metrics_overlay.rs
 *     injected into `.canvas-container` by canvas_generator.rs (grid + split arms).
 *
 * A real second camera-on browser is required so the peer tile actually decodes
 * video (mock peers are video-OFF placeholders that never decode). Cameras AND
 * mics both default OFF, so `vc_prejoin_camera_on` and `vc_prejoin_mic_on` are
 * seeded to `"true"` for both users — camera-on drives the RECEIVED-video
 * metrics, and mic-on makes the peer's audio field visible for the #2279
 * assertion that it renders the em-dash ("· —k").
 * Untagged (no @bvt): runs in the dioxus full suite, not per-PR CI.
 *
 * ⚠ THIS SPEC CANNOT CURRENTLY RUN GREEN LOCALLY — issue #2193, and it is NOT the
 * #2170 assertion below that fails. `setupTwoUserMeeting` times out waiting for the
 * remote peer's decoding canvas tile (`.grid-item:has(canvas)`, 30s), so the test
 * dies in the harness before reaching any assertion. Verified independent of #2170:
 * reverting this file to its PR-staging baseline fails identically, and the
 * untouched `crop-toggle.spec.ts` fails on the SAME locator (4 of 5 tests). A
 * from-scratch stack rebuild (`make e2e-down` with volumes + `make e2e-up`) did not
 * fix it, and single-peer specs on the same stack are green.
 *
 * So the #2170 self-overlay assertion added here is WRITTEN AND LINTED BUT NOT YET
 * DEMONSTRATED GREEN. It is NOT the only guard on that consumer, and the others ARE
 * proven green:
 *   - `performance-settings.spec.ts::"self-tile overlay reports the FITTED encode
 *     size, never the AQ tier box"` — the SAME `host.rs::self_metrics_overlay`
 *     consumer on the real production path (live encoder → publish → snapshot →
 *     rendered DOM), in a SOLO meeting, because the self overlay needs no remote
 *     peer. Runs green, and mutation-verified: restoring the tier-box readout fails
 *     it with "reported 1920×1080".
 *   - `dioxus-ui/tests/send_video_readout_unknown_dims.rs` (wasm, rendered DOM, per-PR CI)
 *
 * NOT a guard, deliberately not listed above:
 * `media_metrics_overlay.rs::self_line_renders_an_em_dash_for_unpublished_encode_geometry`
 * calls `format_media_metrics_line(true, None, …)` — a pure formatter #2170 does not
 * change — with a hand-passed `None`. Reverting `live_quality_snapshot` to the tier
 * box leaves it GREEN (verified by running that mutation). It usefully DOCUMENTS a
 * state #2170 newly made reachable on the self tile, which is why it exists, but it
 * cannot detect a regression in this change and must not be cited as if it could.
 *
 * What ONLY this spec can add once #2193 is fixed is the 2-peer part: with a real
 * receiver the AQ can earn rungs above the base, so `top_published_layer_dims`'
 * `[..active]` bound and `.rev()` scan are exercised non-degenerately end-to-end.
 * In a solo meeting the relay's layer-union hint pins `active = 1` and both reduce
 * to "read slot 0" (host tests cover them at the unit level meanwhile). Re-run this
 * spec once #2193 lands.
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
  "--auto-select-desktop-capture-source=Entire screen",
];

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") return;

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Two-user meeting with real camera-on peers. Seeds `vc_prejoin_camera_on` so
 * both join with camera enabled (default is OFF), producing a decoding peer
 * tile — required for the RECEIVED-video overlay metrics.
 * (Pattern: crop-toggle.spec.ts::setupTwoUserMeeting.)
 */
async function setupTwoUserMeeting(
  uiURL: string,
  meetingId: string,
  opts: { threeRungLadder?: boolean } = {},
) {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    "metricshost@videocall.rs",
    "MetricsHost",
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    "metricsguest@videocall.rs",
    "MetricsGuest",
    uiURL,
  );

  // Issue #2170: force a THREE-rung camera ladder on BOTH peers, before the first
  // navigation (the `**/config.js` route is context-scoped and must be installed
  // before the very first request).
  //
  // Load-bearing, not incidental. The committed `dioxus-ui/scripts/config.js` ships
  // `experimentalSimulcastMaxLayers: 1`, so WITHOUT this the encoder has exactly one
  // published slot and the self-overlay assertion cannot exercise
  // `top_published_layer_dims`' `[..active]` bound or its `.rev()` downward scan —
  // both collapse to "read slot 0". Since this is the suite's only two-peer harness,
  // it is also the only place the AQ can earn a rung above the base at all: a solo
  // meeting has no receiver, so the relay's layer-union hint pins `active = 1`.
  //
  // `capabilityMaxLayersOverride` is required alongside the flag because the real
  // ceiling is `min(flag, sniffed capability)` and a low-core CI runner clamps to 1,
  // which would silently restore the degenerate single-slot case.
  // OPT-IN (`threeRungLadder`), not unconditional: a 3-rung ladder triples this
  // peer's encode load, and only the #2170 test needs it. Forcing it on every test in
  // the file would tax an already-fragile 2-peer harness (see #2193) for tests that
  // never asked for simulcast.
  if (opts.threeRungLadder) {
    await enableSimulcastFlag(hostCtx, 3, { capabilityMaxLayersOverride: 3 });
    await enableSimulcastFlag(guestCtx, 3, { capabilityMaxLayersOverride: 3 });
  }

  // Seed camera-on AND mic-on BEFORE page creation so the initial prejoin read
  // is true for both. Camera-on gives the RECEIVED-video overlay metrics, and
  // mic-on puts the peer audio field in scope. BOTH default OFF
  // (`load_preferred_camera_on` / `load_preferred_mic_on` → false).
  await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_mic_on", "true");`);
  await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_mic_on", "true");`);

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, "MetricsHost");
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, "MetricsGuest");
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Wait for the remote peer tile (with a decoding canvas) on the host grid.
  const canvasTile = hostPage.locator(".grid-item:has(canvas)");
  await expect(canvasTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

/** Open the diagnostics drawer via its tooltip button. */
async function openDiagnosticsPanel(page: Page): Promise<void> {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  await expect(page.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("h3", { hasText: "Transport Preference" })).toBeVisible({
    timeout: 10_000,
  });
}

test.describe("Per-tile media-metrics overlay (issue 1768)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("checkbox toggles per-tile received/sending metric overlays", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `media_metrics_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(uiURL, meetingId);

    try {
      const overlays = hostPage.locator(".media-metrics-overlay");
      const selfOverlay = hostPage.locator('[data-testid="media-metrics-overlay-self"]');
      const peerOverlay = hostPage.locator('[data-testid="media-metrics-overlay-peer"]');

      // Default OFF: no overlay is rendered anywhere before the checkbox is on.
      await expect(overlays).toHaveCount(0);

      await openDiagnosticsPanel(hostPage);

      // The checkbox + its label must exist (fails outright on un-fixed code
      // that lacks the toggle).
      const toggle = hostPage.locator('[data-testid="media-metrics-overlay-toggle"]');
      await expect(toggle).toBeVisible({ timeout: 10_000 });
      await expect(hostPage.locator('label[for="diag-media-metrics-overlay"]')).toHaveText(
        "Show media metrics on tiles",
      );
      // Default OFF and still no overlays with the drawer merely open.
      await expect(toggle).not.toBeChecked();
      await expect(overlays).toHaveCount(0);

      // Turn the overlay ON.
      await toggle.check();
      await expect(toggle).toBeChecked();
      // Production writes the literal string "true"/"false" (local_storage::save_bool).
      await expect
        .poll(() =>
          hostPage.evaluate(() => localStorage.getItem("diagnostics.media_metrics_overlay")),
        )
        .toBe("true");

      // The local user's OWN tile shows its SENDING metrics. Assert visibility
      // BEFORE reading text; every overlay carries the "fps" segment, and the
      // self readout leads with the "↑" (sending) SHAPE cue — the CVD-safe self/
      // peer distinction (issue 1768; self text is white like peers, so colour is
      // never the sole cue).
      await expect(selfOverlay).toBeVisible({ timeout: 20_000 });
      await expect(selfOverlay).toContainText("fps");
      await expect(selfOverlay).toContainText("↑");

      // A REMOTE peer tile shows the RECEIVED metrics, led by the "↓" (receiving)
      // shape cue.
      await expect(peerOverlay.first()).toBeVisible({ timeout: 20_000 });
      await expect(peerOverlay.first()).toContainText("fps");
      await expect(peerOverlay.first()).toContainText("↓");

      // Issue #1784: the received fps is now the PAINTED rate (frames actually drawn
      // to the canvas), delivered per-peer on the decoder's `video_painted`
      // diagnostics event and consumed via `overlay_painted_fps_sample`. This polls
      // that the middle segment is a REAL integer ("· {n}fps ·"), not the em-dash
      // ("· —fps ·") — proving the painted-fps route actually reaches the overlay
      // (a broken route would leave the readout at the em-dash). With the peer's
      // camera seeded on, frames paint, so a positive painted-fps must appear.
      await expect
        .poll(async () => (await peerOverlay.first().textContent()) ?? "", {
          timeout: 30_000,
        })
        .toMatch(/·\s*\d+fps\s*·/);

      // Issue #2279: the RECEIVED-audio field is the trailing "· —k" segment.
      // Pre-#2279 this read `· 12k`/`· 48k`, so the assertion is discriminating;
      // the fps segment above proves the overlay is otherwise live.
      await expect
        .poll(async () => (await peerOverlay.first().textContent()) ?? "", {
          timeout: 30_000,
        })
        .toMatch(/·\s*—k\s*$/);

      // RELOAD PERSISTENCE — guards the `load_bool` read-back the spec would
      // otherwise leave unverified. After a reload the "true" preference must be
      // restored: the flag (seeded from localStorage on mount) drives the overlay
      // WITHOUT reopening the drawer, and the drawer checkbox reads checked.
      await hostPage.reload();
      const grid = hostPage.locator("#grid-container");
      const rejoinBtn = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
      const landed = await Promise.race([
        grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
        rejoinBtn.waitFor({ timeout: 30_000 }).then(() => "join" as const),
      ]);
      if (landed === "join") {
        await rejoinBtn.click();
        await expect(grid).toBeVisible({ timeout: 15_000 });
      }
      // The self-view SENDING overlay reappears once the camera republishes —
      // proof the persisted flag was read back true on mount (no drawer needed).
      await expect(selfOverlay).toBeVisible({ timeout: 30_000 });
      await expect(selfOverlay).toContainText("fps");
      // And the drawer checkbox reflects the persisted "true".
      await openDiagnosticsPanel(hostPage);
      await expect(hostPage.locator('[data-testid="media-metrics-overlay-toggle"]')).toBeChecked();

      // Turn the overlay OFF: every overlay must leave the DOM (conditional
      // render, not visibility toggling) and the preference persists as "false".
      await toggle.uncheck();
      await expect(toggle).not.toBeChecked();
      await expect(overlays).toHaveCount(0, { timeout: 10_000 });
      await expect
        .poll(() =>
          hostPage.evaluate(() => localStorage.getItem("diagnostics.media_metrics_overlay")),
        )
        .toBe("false");
    } finally {
      await browser1.close().catch(() => undefined);
      await browser2.close().catch(() => undefined);
    }
  });
  /**
   * Issue #2170 — the SELF overlay's resolution, with a REAL second peer present.
   *
   * ITS OWN `test()` ON PURPOSE. This block previously sat inline in the test above,
   * between that test's self-overlay assertions and four later blocks (the `↓`
   * peer-overlay assertions, the #1784 painted-fps guard, the #1769 audio-kbps guard,
   * and the reload-persistence block). It ends with a hard 40 s precondition — poll
   * until the AQ reports >= 2 ACTIVE layers — and if that precondition never
   * satisfies, everything after it dies with it, turning previously-independent
   * PROVEN coverage into unreachable code.
   *
   * That risk is not hypothetical. Two premises have to hold for the gate to pass,
   * and neither is guaranteed:
   *   1. `format_video_send_summary` emits "Currently sending N of M layers" ONLY
   *      when `simulcast_active`. Otherwise it emits "Sending single layer · {res}",
   *      which the regex below never matches → a guaranteed 40 s timeout.
   *   2. `active_layers >= 2` must be EARNED by the AQ ramp:
   *      `initial_active_layer_count() == 1`, and promotion is gated on
   *      `LAYER_PROBE_CLEAR_WINDOW_MS = 6000` plus relative-benefit and
   *      uplink-budget checks. That constant's own doc estimates "~11 s to 2 layers"
   *      and marks the figure REASONED, not measured.
   *
   * Isolated here, a failure costs only this test.
   */
  test("self overlay reports the FITTED encode size with a real peer present", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `media_metrics_fitted_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(uiURL, meetingId, {
      threeRungLadder: true,
    });

    try {
      await openDiagnosticsPanel(hostPage);
      const toggle = hostPage.locator('[data-testid="media-metrics-overlay-toggle"]');
      await expect(toggle).toBeVisible({ timeout: 10_000 });
      await toggle.check();

      const selfOverlay = hostPage.locator('[data-testid="media-metrics-overlay-self"]');
      await expect(selfOverlay).toBeVisible({ timeout: 20_000 });

      // Issue #2170: the SELF overlay's resolution is now the geometry the encode
      // loop published for its top ACTIVE layer, NOT the AQ tier's bounding box.
      // `host.rs::self_metrics_overlay` builds it from the same
      // `LiveQualitySnapshot` the perf-drawer readout uses, gating on
      // `video_width > 0 && video_height > 0` — a gate that could NEVER fire before
      // #2170, because a tier box is never zero. So this consumer's behaviour
      // changed twice: the value is now a measurement, and it can legitimately be
      // absent (em-dash) in the pre-first-frame window.
      //
      // WHY THIS SPEC AND NOT THE PERF-DRAWER ONE: it is the only spec that asserts
      // on the SELF-overlay's reported resolution while a real second peer is
      // present. (It is NOT "the only two-peer harness in the suite" — an earlier
      // version of this comment claimed that and it is false: ~28 specs stand up two
      // users, there is a shared `e2e/helpers/two-user-meeting.ts`, and this file's
      // own header credits `crop-toggle.spec.ts` as the pattern it copied.)
      //
      // The reason a second peer matters here is unchanged and does hold: a SOLO
      // meeting has no receiver, so the relay's layer-union hint pins `active = 1`
      // and `top_published_layer_dims`' `[..active]` bound and `.rev()` scan both
      // collapse to "read slot 0". `setupTwoUserMeeting` additionally forces a 3-rung
      // ladder on both peers so they do not collapse for the OTHER reason — without
      // it the committed `experimentalSimulcastMaxLayers: 1` leaves one slot and this
      // assertion exercises neither mechanism. Both conditions are needed; the
      // depth guard below pins the second one.
      //
      // Note the overlay uses `×` (U+00D7) while the perf readout uses ASCII `x` —
      // different formatters, deliberately not unified here.
      //
      // THE ASSERTION IS ON THE ASPECT RATIO, NOT AN ENUMERATED VALUE, and that is
      // deliberate. Which rung the AQ has earned moves during the poll window (the
      // ramp is time- and link-dependent), so the reported pair is legitimately any
      // of the three rungs' fits — `240×180 / 480×360 / 640×480` on this ladder.
      // Pinning that list would re-break whenever a rung's box changes, and would
      // also have to change if the ladder variant did.
      //
      // The INVARIANT is what discriminates: `fit_within_preserving_aspect` only ever
      // SHRINKS and always preserves the source aspect, and Chromium's
      // `--use-fake-device-for-media-stream` capture is 4:3. Every simulcast rung AND
      // every `VIDEO_QUALITY_TIERS` entry is 16:9. So "the reported pair is 4:3" holds
      // for every legitimate fitted value at every rung, and fails for every box —
      // which is exactly the defect being guarded, with no list to maintain.
      //
      // MUTATION, RUN rather than predicted: reverting `live_quality_snapshot` to
      // `video_width: v.max_width, video_height: v.max_height` makes this report a
      // 16:9 box and the ratio assertion fails.
      // NON-DEGENERACY PREMISE — gate the capture on the LIVE ACTIVE COUNT, and note
      // this MUST come before the overlay capture below, not after it.
      //
      // An earlier version of this guard asserted 3 `.perf-rung` pips, AFTER
      // capturing the overlay. Wrong twice. It was VACUOUS: `layer_send_rungs` builds
      // one pip per LADDER label with `active: i <= ceiling_pos` — off the ladder
      // depth and the USER's ceiling, never off the AQ's live active count. So it
      // rendered 3 pips at `active == 1` too, and the geometry assertions would have
      // passed even if `top_published_layer_dims` always returned slot 0 or its
      // `.rev()` scan were broken. It proved the simulcast flag took effect, nothing
      // more. And asserting a premise after the value that depends on it has already
      // been sampled cannot constrain that sample at all.
      //
      // The video summary line carries the real signal: `format_video_send_summary`
      // renders "Currently sending {active_layers} of {effective_layers} layers" from
      // the snapshot's shed-aware `active_layers`. Requiring >= 2 ACTIVE before
      // sampling means the `[..active]` bound has a rung to exclude and the downward
      // scan has more than one slot to walk.
      //
      // The diagnostics drawer is ALREADY open from the `openDiagnosticsPanel` call
      // above (it is what exposed the overlay toggle), and the perf panel mounts
      // inside it, so this locator resolves. Deliberately NOT re-opening it — that
      // clicks the same tooltip button and would TOGGLE the drawer shut.
      const videoSummary = hostPage.locator(
        '[data-testid="perf-video-range-value"] + .perf-summary-line',
      );
      await expect
        .poll(
          async () => {
            const t = (await videoSummary.innerText().catch(() => "")) ?? "";
            const m = t.match(/sending\s+(\d+)\s+of\s+(\d+)\s+layers/i);
            return m ? Number(m[1]) : 0;
          },
          { timeout: 40_000 },
        )
        .toBeGreaterThan(1);

      // Only NOW sample the overlay — with >= 2 rungs active, the reported geometry
      // is the top ACTIVE rung's, so the bound and the scan are both load-bearing.
      let selfDims: { w: number; h: number } | null = null;
      await expect
        .poll(
          async () => {
            const text = (await selfOverlay.textContent()) ?? "";
            // The self readout is `"↑ {w}×{h} · {fps}fps · {audio}k"`. An absent
            // resolution renders an em-dash, which this pattern rejects — so the
            // poll also waits out the pre-first-frame window rather than asserting
            // against it.
            const m = text.match(/↑\s*(\d+)×(\d+)\s*·/);
            selfDims = m ? { w: Number(m[1]), h: Number(m[2]) } : null;
            return selfDims;
          },
          { timeout: 30_000 },
        )
        .not.toBeNull();

      const { w, h } = selfDims!;
      // 4:3 within a rounding tolerance: `round_down_even` floors each axis to an
      // even number, so an exact ratio is not guaranteed (e.g. 360×270 is exact,
      // but an odd-axis fit is nudged down by a pixel).
      expect(
        Math.abs(w / h - 4 / 3),
        `the self overlay reported ${w}×${h}; a fitted 4:3 capture must stay 4:3, ` +
          `whereas every simulcast rung and AQ tier box is 16:9 (${(16 / 9).toFixed(3)})`,
      ).toBeLessThan(0.05);
      // Belt-and-braces: fitting never UPSCALES, so the reported geometry can never
      // exceed the 640x480 capture. This catches a box that happens to be near-4:3
      // as well as any future upscaling regression.
      expect(w, "fitting never upscales beyond the capture width").toBeLessThanOrEqual(640);
      expect(h, "fitting never upscales beyond the capture height").toBeLessThanOrEqual(480);
    } finally {
      await browser1.close().catch(() => undefined);
      await browser2.close().catch(() => undefined);
    }
  });
});
