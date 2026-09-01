/**
 * E2E: the relay's VIEWPORT filter follows the receiver's on-screen LAYOUT roster,
 * not its local decode-budget cap (issue 2602; filter is #988/PR #994, both-transport
 * coverage is #995).
 *
 * PHASE 1 is the only 2602 guard: budget floored to MIN_CAP with both tiles on screen,
 * `filtered` FLAT while `forwarded` CLIMBS. `forwarded` is room-scoped, so its climb
 * shows the room is flowing, not that this receiver's own subscription is.
 * PHASE 2 is the retained #995/#988 drop-check and passes on un-fixed code too: one
 * publisher folded into `+N` with no canvas, `filtered` CLIMBS.
 * PHASE 3 is the only guard on the widen-republish path — back to full size, FLAT again.
 *
 * Publishers join MUTED because `compute_effective_density` escalates density while
 * a remote peer counts as an active speaker; unmuted, PHASE 2's `+N` region does not
 * form.
 *
 * The `reset_for_reconnect` re-send is deliberately NOT covered — no per-client
 * transport-drop rig exists on either transport, and the normal debounced send masks
 * it (#1022, #1355). Pure logic is covered by `viewport_sender.rs` unit tests.
 */

import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { readViewportFilteredTotal, readViewportForwardedTotal } from "../helpers/relay-metrics";
import { BUDGET, DENSITY } from "../helpers/rust-mirrored-constants";

// A synthetic FPS in the MILD band — strictly ABOVE FPS_SEVERE (so each
// down-step drops a SINGLE tile, not a proportional multi-tile burst) and below
// FPS_STEP_DOWN (so it qualifies as pressure). With only 2 remote publishers a
// severe multi-tile drop would push BOTH off-budget at once (0 decoded), which
// both overshoots the "exactly one excluded" target AND transiently violates
// the MIN_CAP=1 rendered floor; single-tile steps land deterministically at
// 1 decoded + 1 off-budget. Mirrors decode-budget.spec.ts LOW_FPS rationale.
const MILD_LOW_FPS = BUDGET.FPS_STEP_DOWN - 6; // 18: < FPS_STEP_DOWN, > FPS_SEVERE
// Slightly above the loop's ~1 Hz bucket cadence so each injection lands in a
// fresh bucket (matches decode-budget.spec.ts INJECT_INTERVAL_MS).
const INJECT_INTERVAL_MS = 1200;
// Bounded number of mild samples to step the cap down by one tile. Needs
// SUSTAIN_SAMPLES (3) + a couple of STEP_DOWN_COOLDOWN_MS windows; generous
// headroom for CI jitter.
const MAX_DRIVE_SAMPLES = 14;
// Dwell long enough that `filtered` is sampled over real time rather than fetch latency,
// in ticks so the caller can keep its premise true throughout.
const DWELL_TICKS = 4;
const DWELL_TICK_MS = 1_000;
// A 2-tile layout measures 272px here, under Standard's minimum, so the grid seats one
// and folds the other into `+1`. Locked against the mirrored Rust values.
const OVERFLOW_VIEWPORT = { width: 600, height: 500 };
const TWO_TILE_WIDTH_AT_OVERFLOW_VIEWPORT = 272;
const TWO_TILE_WIDTH_AT_FULL_VIEWPORT = 612;
// Both tiles on screen, so the budget floor is the only narrowing. Explicit, not
// inherited — it is a geometry precondition.
const FULL_VIEWPORT = { width: 1280, height: 720 };

/** The two transports the relay-drop assertion is parameterised over. */
type Transport = "websocket" | "webtransport";

// ---------------------------------------------------------------------------
// Helpers (the join flow mirrors the proven 3-context flow in
// diagnostics-peer-transport.spec.ts / simulcast-per-receiver.spec.ts).
// ---------------------------------------------------------------------------

/** Whether the test-only FPS injection hook is attached (MOCK_PEERS_ENABLED). */
const hasInjectHook = (page: Page) =>
  page.evaluate(
    () =>
      typeof (window as unknown as { __videocall_inject_render_fps?: unknown })
        .__videocall_inject_render_fps === "function",
  );

/** Inject one synthetic render-fps sample (closes one ~1 Hz bucket in the loop). */
const injectFps = (page: Page, fps: number) =>
  page.evaluate(
    (v) =>
      (
        window as unknown as { __videocall_inject_render_fps: (n: number) => void }
      ).__videocall_inject_render_fps(v),
    fps,
  );

// Decoded remote tiles vs off-budget avatar tiles (real peers only — mock tiles
// are layout-only and never reach here in this spec). Mirrors the selectors in
// decode-budget.spec.ts and canvas_generator.rs (data-off-budget / off-budget-tile).
const decodedTiles = (page: Page) =>
  page.locator('#grid-container .grid-item[data-off-budget="false"]');
const offBudgetTiles = (page: Page) => page.locator("#grid-container .grid-item.off-budget-tile");
// Present only for peers OUTSIDE the layout capacity — no canvas (`GridOverflowBadge`).
const overflowBadge = (page: Page) => page.locator("#grid-container .grid-overflow-badge");

async function seedStandardDensity(context: BrowserContext) {
  await context.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_density_mode", "standard");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });
}

/**
 * Pin a BrowserContext to a specific media transport BEFORE its first
 * navigation by seeding the sticky preference the UI reads from localStorage at
 * boot (context.rs). `createAuthenticatedContext` only sets a WS default when no
 * preference exists, so seeding here (added AFTER that init script, but it sets
 * unconditionally) wins. Mirrors the cross-transport pin in
 * cross-transport-display-name.spec.ts.
 */
async function pinTransport(context: BrowserContext, t: Transport) {
  const pref = t === "webtransport" ? "webtransport" : "websocket";
  await context.addInitScript((p: string) => {
    try {
      window.localStorage.setItem("vc_transport_preference", p);
      window.localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  }, pref);
}

/**
 * Drive a fresh page from the HOME FORM into the meeting grid with the camera ON
 * (so a publisher actually emits VIDEO through the relay — required for the
 * relay-drop counter to move). Mirrors `joinMeeting` in
 * simulcast-per-receiver.spec.ts: seed camera/mic ON before boot, type the
 * meeting id + display name on the home form, then race the pre-join Start/Join
 * button against the grid, disabling the Waiting Room on the host's card so
 * later joiners are auto-admitted.
 */
async function joinMeeting(
  page: Page,
  meetingId: string,
  displayName: string,
  micOn = true,
): Promise<void> {
  await page.addInitScript(
    (mic: string) => {
      try {
        window.localStorage.setItem("vc_prejoin_camera_on", "true");
        window.localStorage.setItem("vc_prejoin_mic_on", mic);
      } catch {
        /* storage may be unavailable pre-navigation; the app origin sets it */
      }
    },
    micOn ? "true" : "false",
  );

  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
    const allow = page.locator('[data-testid="prejoin-permission-allow"]');
    if (await allow.isVisible().catch(() => false)) {
      await allow.click();
      await page
        .locator('[data-testid="prejoin-permission-prompt"]')
        .waitFor({ state: "hidden", timeout: 15_000 })
        .catch(() => {
          /* already granted / prompt absent */
        });
    }

    // HOST ONLY: disable the Waiting Room so later joiners are auto-admitted
    // straight into the grid (the toggle is rendered only on the owner's card).
    const waitingRoomRow = page.locator(".settings-option-row", {
      has: page.getByText("Waiting Room", { exact: true }),
    });
    const waitingRoomToggle = waitingRoomRow.getByRole("switch");
    if (await waitingRoomToggle.isVisible().catch(() => false)) {
      let settled: string | null = null;
      await expect
        .poll(
          async () => {
            const first = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            await page.waitForTimeout(250);
            const second = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            if (first !== null && first === second) {
              settled = second;
              return true;
            }
            return false;
          },
          { timeout: 10_000, intervals: [250, 500] },
        )
        .toBe(true)
        .catch(() => {
          /* never settled within budget — fall through without toggling */
        });
      if (settled === "true") {
        await waitingRoomToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
        await expect(waitingRoomToggle).toHaveAttribute("aria-checked", "false", {
          timeout: 10_000,
        });
      }
    }

    // Ensure the camera is ON before joining so this context publishes video.
    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
        await cameraToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
      await expect
        .poll(
          async () =>
            page
              .locator('[data-testid="prejoin-camera-preview"]')
              .evaluate((el) => {
                const v = el as HTMLVideoElement;
                const s = v.srcObject as MediaStream | null;
                return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
              })
              .catch(() => 0),
          { timeout: 15_000 },
        )
        .toBeGreaterThan(0);
    }

    await page.waitForTimeout(500);
    await joinButton.click().catch(() => {
      /* auto-join already unmounted the pre-join button */
    });
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/** Open Device Settings → Preferences tab so the decode-budget control shows. */
async function openPreferencesPanel(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
  await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 5_000 });
  await expect(page.locator("#decode-budget-override")).toBeVisible({ timeout: 5_000 });
}

async function closeSettingsModal(page: Page): Promise<void> {
  await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
}

/** Put the receiver's decode budget in Auto so the adaptive loop owns the cap. */
async function selectAutoBudget(page: Page): Promise<void> {
  await openPreferencesPanel(page);
  await page.locator('[data-testid="decode-budget-auto"]').click();
  await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
    "aria-checked",
    "true",
  );
  await closeSettingsModal(page);
}

/** A retune of either mirrored value silently changes which window overflows. */
function assertOverflowGeometryStillHolds(): void {
  expect(OVERFLOW_VIEWPORT.width).toBeGreaterThan(DENSITY.MOBILE_WIDTH_BREAKPOINT_PX);
  expect(FULL_VIEWPORT.width).toBeGreaterThan(DENSITY.MOBILE_WIDTH_BREAKPOINT_PX);
  expect(TWO_TILE_WIDTH_AT_OVERFLOW_VIEWPORT).toBeLessThan(
    DENSITY.STANDARD_MIN_TILE_WIDTH_DESKTOP_PX,
  );
  expect(TWO_TILE_WIDTH_AT_FULL_VIEWPORT).toBeGreaterThan(
    DENSITY.STANDARD_MIN_TILE_WIDTH_DESKTOP_PX,
  );
}

async function driveBudgetToFloor(page: Page): Promise<void> {
  // A retune can flip MILD_LOW_FPS into the SEVERE band, where a down-step drops
  // BOTH publishers at once rather than the single tile this asserts.
  expect(MILD_LOW_FPS).toBeGreaterThan(BUDGET.FPS_SEVERE);
  for (let i = 0; i < MAX_DRIVE_SAMPLES; i++) {
    if ((await offBudgetTiles(page).count()) > 0) break;
    await injectFps(page, MILD_LOW_FPS);
    await page.waitForTimeout(INJECT_INTERVAL_MS);
  }
  await expect(
    offBudgetTiles(page),
    "the receiver must push at least one real publisher off-budget (the local decode gate shrank; the VIEWPORT must NOT)",
  ).not.toHaveCount(0, { timeout: 15_000 });
  // MIN_CAP keeps at least one remote tile decoded. Poll (not an instantaneous
  // read) because the off-budget transition re-lays-out the grid and the
  // decoded count can momentarily read 0 mid-render before the cap re-clamps.
  await expect(decodedTiles(page), "MIN_CAP keeps one remote tile decoded").not.toHaveCount(0, {
    timeout: 15_000,
  });
}

/** Poll a relay viewport counter until it rises by `minDelta` above `from`. */
async function expectCounterToClimb(
  read: (t: Transport, room: string) => Promise<number>,
  transport: Transport,
  room: string,
  from: number,
  minDelta: number,
  timeoutMs: number,
  message: string,
): Promise<number> {
  let latest = from;
  await expect
    .poll(
      async () => {
        latest = await read(transport, room);
        return latest;
      },
      { timeout: timeoutMs, intervals: [500, 1000, 2000], message },
    )
    .toBeGreaterThanOrEqual(from + minDelta);
  return latest;
}

/** Poll until `filtered` stops moving, so a later flat assertion is not sampled mid-transition. */
async function expectFilteredToSettle(transport: Transport, room: string): Promise<void> {
  let previous = -1;
  await expect
    .poll(
      async () => {
        const current = await readViewportFilteredTotal(transport, room);
        const settled = current === previous;
        previous = current;
        return settled;
      },
      {
        timeout: 30_000,
        intervals: [1000, 1000, 2000],
        message: `relay_viewport_filtered_total on the ${transport} relay must stop climbing once the widened viewport lands`,
      },
    )
    .toBe(true);
}

/** `filtered` must not move over a window in which `forwarded` demonstrably does. */
async function expectFilteredFlatWhileForwardedClimbs(
  transport: Transport,
  room: string,
  minForwardedDelta: number,
  timeoutMs: number,
  premise?: { sustain: () => Promise<void>; assert: () => Promise<void> },
): Promise<void> {
  // Hold the caller's premise for the WHOLE helper, not just the dwell. The forwarded-climb
  // poll below has a 30s budget and applies no pressure, so a floor established once has
  // already recovered by the time the dwell starts (observed: off-budget count 0 at the
  // re-read even with the dwell sustaining).
  let sustaining = premise !== undefined;
  const sustainLoop = (async () => {
    while (sustaining) {
      await premise?.sustain().catch(() => {
        /* page may be mid-navigation; the next tick retries */
      });
      await new Promise((resolve) => setTimeout(resolve, DWELL_TICK_MS));
    }
  })();

  try {
    const filteredBefore = await readViewportFilteredTotal(transport, room);
    const forwardedBefore = await readViewportForwardedTotal(transport, room);
    await expectCounterToClimb(
      readViewportForwardedTotal,
      transport,
      room,
      forwardedBefore,
      minForwardedDelta,
      timeoutMs,
      `the ${transport} relay must keep FORWARDING this room's video while the receiver sits at the ` +
        "decode-budget floor — otherwise a flat `filtered` proves nothing",
    );
    // Dwell: `forwarded` counts per packet per receiver across three layers, so the climb
    // target is often met by the first poll. Hold the window open so `filtered` is sampled
    // over real time — and re-check `forwarded` AFTER the sleep, or a stall during the dwell
    // makes the flat reading meaningless again.
    const forwardedMidDwell = await readViewportForwardedTotal(transport, room);
    await new Promise((resolve) => setTimeout(resolve, DWELL_TICKS * DWELL_TICK_MS));
    // Bare read, not a poll: the claim is "forwarding was alive ACROSS the dwell", and a
    // 15s-budget poll would only prove "alive somewhere in the next 15s".
    expect(
      await readViewportForwardedTotal(transport, room),
      `the ${transport} relay must still be forwarding across the dwell window`,
    ).toBeGreaterThan(forwardedMidDwell);
    // Re-assert HERE, not before the dwell: this is where `filtered` is read.
    if (premise) {
      await premise.assert();
    }
    const filteredAfter = await readViewportFilteredTotal(transport, room);
    expect(
      filteredAfter,
      `relay_viewport_filtered_total on the ${transport} relay must stay FLAT while the receiver is at ` +
        "the decode-budget floor with both tiles on screen — a climb means the local cap was published " +
        "as the viewport and blacked out the other publisher (issue 2602)",
    ).toBe(filteredBefore);
  } finally {
    sustaining = false;
    await sustainLoop;
  }
}

// ---------------------------------------------------------------------------
// Parameterised relay-drop observation over BOTH transports.
// ---------------------------------------------------------------------------

for (const transport of ["websocket", "webtransport"] as const) {
  test.describe(`Viewport follows the layout, not the decode cap, over ${transport} (2602/#995/#988)`, () => {
    // Three heavy WebCodecs renderers (1 receiver + 2 publishers). Serial +
    // generous timeout for the same renderer-footprint reason as
    // simulcast-per-receiver.spec.ts.
    test.describe.configure({ mode: "serial", timeout: 240_000 });

    test.beforeAll(async () => {
      assertOverflowGeometryStillHolds();
      await waitForServices();
    });

    test(`the viewport follows the layout, not the decode cap, on the ${transport} path`, async ({
      baseURL,
    }) => {
      const uiURL = baseURL || "http://localhost:3001";
      const tag = transport === "webtransport" ? "wt" : "ws";
      const meetingId = `e2e_vp_filter_${tag}_${Date.now()}`;

      const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
      const pub1Browser: Browser = await chromium.launch({ args: BROWSER_ARGS });
      const pub2Browser: Browser = await chromium.launch({ args: BROWSER_ARGS });
      try {
        const rxCtx = await createAuthenticatedContext(
          rxBrowser,
          `vp-${tag}-rx@videocall.rs`,
          "VpReceiver",
          uiURL,
        );
        const pub1Ctx = await createAuthenticatedContext(
          pub1Browser,
          `vp-${tag}-pub1@videocall.rs`,
          "VpPublisher1",
          uiURL,
        );
        const pub2Ctx = await createAuthenticatedContext(
          pub2Browser,
          `vp-${tag}-pub2@videocall.rs`,
          "VpPublisher2",
          uiURL,
        );

        // Pin all three contexts to the transport under test so the VIDEO is
        // forwarded by the matching relay process (whose /metrics we scrape).
        for (const ctx of [rxCtx, pub1Ctx, pub2Ctx]) {
          await pinTransport(ctx, transport);
        }
        await seedStandardDensity(rxCtx);

        const rxPage = await rxCtx.newPage();
        const pub1Page = await pub1Ctx.newPage();
        const pub2Page = await pub2Ctx.newPage();

        // Receiver is the first joiner (host) so it can disable the Waiting Room;
        // publishers join after and are auto-admitted.
        await joinMeeting(rxPage, meetingId, "VpReceiver");
        // MUTED — see the header. Audio is irrelevant: the filter is camera-VIDEO only.
        await joinMeeting(pub1Page, meetingId, "VpPublisher1", false);
        await joinMeeting(pub2Page, meetingId, "VpPublisher2", false);

        if (!(await hasInjectHook(rxPage))) {
          test.skip(
            true,
            "window.__videocall_inject_render_fps not registered (MOCK_PEERS_ENABLED off)",
          );
          return;
        }

        // The receiver must actually see BOTH publishers' tiles before we can
        // push one off-screen.
        await expect(rxPage.locator("#grid-container .canvas-container")).toHaveCount(2, {
          timeout: 45_000,
        });

        // Confirm video is reaching THIS relay's filter for this room — proves
        // the contexts elected the intended transport (if they fell back, the
        // matching relay's forwarded counter stays 0 and the test is invalid
        // rather than silently green on the wrong path).
        await expect
          .poll(() => readViewportForwardedTotal(transport, meetingId), {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
            message:
              `the ${transport} relay must be forwarding (=deciding on) this room's VIDEO before we ` +
              "filter; if it stays 0 the contexts did not elect this transport and the test is invalid",
          })
          .toBeGreaterThan(0);

        // ---- PHASE 1 (2602): the decode floor must not narrow the viewport.
        await rxPage.setViewportSize(FULL_VIEWPORT);
        await selectAutoBudget(rxPage);
        await driveBudgetToFloor(rxPage);
        await expect(
          overflowBadge(rxPage),
          "PHASE 1 precondition: at full size no peer is off SCREEN — the budget floor must be the " +
            "only thing narrowed, so the +N badge must be absent",
        ).toHaveCount(0);
        await expectFilteredFlatWhileForwardedClimbs(transport, meetingId, 5, 30_000, {
          sustain: () => injectFps(rxPage, MILD_LOW_FPS),
          assert: async () => {
            await expect(
              offBudgetTiles(rxPage),
              "PHASE 1 premise must still hold when `filtered` is re-read: the budget must not " +
                "have recovered off the floor during the dwell",
            ).not.toHaveCount(0);
          },
        });

        // ---- PHASE 2 (#995/#988): a genuinely off-screen peer IS dropped.
        await rxPage.setViewportSize(OVERFLOW_VIEWPORT);
        await expect(
          overflowBadge(rxPage),
          "PHASE 2 precondition: the narrowed window must fold one publisher into the +N badge " +
            "(no canvas, genuinely off screen)",
        ).toHaveCount(1, { timeout: 20_000 });
        await expect(overflowBadge(rxPage)).toHaveAttribute("data-overflow-count", "1");

        const filteredBefore = await readViewportFilteredTotal(transport, meetingId);
        await expectCounterToClimb(
          readViewportFilteredTotal,
          transport,
          meetingId,
          filteredBefore,
          5,
          30_000,
          `relay_viewport_filtered_total on the ${transport} relay must climb once a publisher is ` +
            "genuinely off screen — the transport-agnostic drop-check firing on this path (#995)",
        );

        // ---- PHASE 3: repeat PHASE 1 now that a viewport is PROVEN established.
        // `forwarded` also increments on the relay's fail-open path, so phase 1 alone is
        // satisfied by a client that never published a viewport at all.
        await rxPage.setViewportSize(FULL_VIEWPORT);
        await expect(overflowBadge(rxPage)).toHaveCount(0, { timeout: 20_000 });
        // The badge clearing is a DOM fact; the relay keeps dropping until the widened
        // viewport reaches it (debounce + RTT + packets already in flight). Wait for the
        // counter to settle before sampling, or the baseline is taken mid-transition.
        await expectFilteredToSettle(transport, meetingId);
        await expectFilteredFlatWhileForwardedClimbs(transport, meetingId, 5, 30_000);
      } finally {
        await rxBrowser.close();
        await pub1Browser.close();
        await pub2Browser.close();
      }
    });
  });
}
