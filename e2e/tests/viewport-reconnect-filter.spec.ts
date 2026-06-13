/**
 * E2E: viewport-aware relay video filtering, validated over BOTH transports
 * (HCL issue #995; the feature itself is issue #988 / PR #994).
 *
 * ## What this guards
 *
 * The relay drops a publisher's off-screen VIDEO when that publisher's
 * session_id is not in the RECEIVER's VIEWPORT set, incrementing the dedicated
 * Prometheus counter `relay_viewport_filtered_total{room}` (see
 * `actix-api/src/actors/chat_server.rs::handle_msg` — the per-receiver fan-out
 * closure — and `RELAY_VIEWPORT_FILTERED_TOTAL` in `actix-api/src/metrics.rs`).
 * The viewport set is sent by the client when the rendered set of peers changes:
 * `set_active_decode_set` → `ViewportSender::record` → (debounced) →
 * `send_viewport_via` (`videocall-client/src/client/video_call_client.rs`).
 *
 * This spec asserts the feature's only AUTHORITATIVE server-side effect: that
 * the relay ACTUALLY DROPS an off-screen real publisher's VIDEO, observed by
 * scraping `relay_viewport_filtered_total` (see `helpers/relay-metrics.ts`).
 * It validates this on BOTH the WebSocket relay (:8080) and the WebTransport
 * relay (:5321), because the drop-check is transport-agnostic and runs inside
 * BOTH relay binaries (each with its own metrics registry). This is the
 * coverage issue #995 asks for that was previously verifiable only by
 * inspection — the relay-side observable effect of the viewport filter, on
 * both forwarding paths.
 *
 * ## Why a relay metric, not a DOM check
 *
 * A DOM-only check (`data-off-budget="true"`) proves the CLIENT shrank its
 * decode set, but NOT that the relay acted on it. `relay_viewport_filtered_total`
 * is the only server-side proof the relay genuinely dropped the video. This
 * spec asserts BOTH: the DOM signal (the client excluded the publisher) AND the
 * relay counter (the relay dropped that publisher's video).
 *
 * ## Fails if the feature regresses
 *
 * If the client stops sending the VIEWPORT (e.g. `set_active_decode_set` /
 * `send_viewport_via` removed) OR the relay stops honouring it (the `handle_msg`
 * drop-check removed), the relay's viewport set for the receiver stays empty →
 * fail-open → ALL video forwarded → `relay_viewport_filtered_total` stays FLAT →
 * the climb assertion fails. The DOM off-budget assertion separately fails if
 * the client never excludes a publisher.
 *
 * ## Scope note — the RECONNECT-specific re-send (`reset_for_reconnect`)
 *
 * Issue #995's PREFERRED form is an assertion that filtering RESUMES after a
 * transport drop + reconnect (the wasm-only `Connected`-arm re-send
 * `reset_for_reconnect()` → `send_viewport_via`). That isolation was attempted
 * and found INFEASIBLE to make a genuine fail-when-removed guard with the
 * current harness, for two independent reasons established by real runs against
 * the Docker stack:
 *
 *   1. NO RELIABLE PER-CLIENT TRANSPORT DROP. toxiproxy (TCP) can sever a WS,
 *      but routing the media WS through it depends on a `/config.js` `wsUrl`
 *      patch that the e2e stack's comment-prefixed config defeats — the receiver
 *      connected directly to the relay (`server_url: ws://localhost:8080/lobby`)
 *      and the proxy sever had no effect, so no reconnect occurred. netsim only
 *      drops packets (cannot disconnect), and there is no client-side
 *      force-reconnect hook. WT/QUIC has no per-client impairment at all.
 *
 *   2. NORMAL-PATH MASKING. Even with a real reconnect, the `Connected` arm
 *      clears peers (`clear_all_peers` on `Failed`) and the post-reconnect grid
 *      re-render recomputes `active_decode_set`, so the NORMAL debounced path
 *      (`record` → `send_viewport_via`) re-sends the viewport on its own and
 *      re-establishes filtering — masking the SPECIFIC `reset_for_reconnect`
 *      re-send. A relay-metric assertion therefore cannot attribute the
 *      post-reconnect climb to the reconnect glue rather than the normal send.
 *
 * Building a reconnect rig that (a) reliably severs ONE client's transport on
 * both WS and WT and (b) holds the active_decode_set stable across the
 * reconnect so the normal path stays silent is new diagnostics/impairment infra
 * — a separate, larger task (cf. issue #1022). Shipping a reconnect test that
 * passes WHETHER OR NOT the re-send exists (verified: it did) would be a
 * false-green guard, so it is deliberately NOT included. The `reset_for_reconnect`
 * pure logic remains covered by the `viewport_sender.rs` unit tests
 * (`reconnect_forces_resend_of_current_set` et al.); the host-level glue
 * (`Connected` arm) is wasm-only `Callback`/`Rc`/`web_sys`, and — per the masking
 * in (2) — its only observable effect (the relay drop) cannot be attributed to it
 * rather than the normal debounced send, so an e2e guard for it specifically
 * awaits the reconnect-isolation rig described above.
 *
 * ## Topology & why FPS injection
 *
 * Mock peers are layout-only and flow NO real video through the relay, so they
 * cannot trigger a relay drop. We use REAL publishers: 1 receiver + 2 camera-on
 * publishers (3 contexts — the count `diagnostics-peer-transport.spec` already
 * runs). To push a REAL publisher OUT of the receiver's viewport we step the
 * adaptive decode budget down to its floor (`MIN_CAP = 1`) by injecting
 * sustained MILD-low FPS via the test-only `window.__videocall_inject_render_fps`
 * hook (gated on `MOCK_PEERS_ENABLED`, which the e2e stack sets). With 2 remote
 * publishers and cap 1, exactly one publisher becomes an off-budget avatar →
 * removed from `active_decode_set` → VIEWPORT excludes it → relay drops its
 * VIDEO. (The smallest selectable FIXED budget is 4 and it clamps UP to the
 * natural tile count, so a fixed cap can never exclude anyone with only 2
 * publishers; the adaptive floor is the deterministic low-context lever. MILD —
 * not severe — pressure is used so the loop drops a SINGLE tile per step: a
 * severe multi-tile drop would push BOTH remote tiles off at once, overshooting
 * the "exactly one excluded" target.)
 */

import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { readViewportFilteredTotal, readViewportForwardedTotal } from "../helpers/relay-metrics";

// ---------------------------------------------------------------------------
// Decode-budget control loop constants — mirrored from
// dioxus-ui/src/components/decode_budget.rs. Kept in lockstep like
// decode-budget.spec.ts (which copies the same block); if those consts are
// retuned this spec must follow.
// ---------------------------------------------------------------------------
const BUDGET = {
  FPS_STEP_DOWN: 24, // FPS at/below which the loop considers stepping DOWN
  FPS_SEVERE: 12, // median FPS at/below which a down-step drops MULTIPLE tiles
  MIN_CAP: 1, // adaptive floor: at least one remote tile always decodes
} as const;

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
async function joinMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

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

/** Open Device Settings → Appearance tab so the decode-budget control shows. */
async function openAppearancePanel(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator(".settings-nav-button").filter({ hasText: "Appearance" }).click();
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
  await expect(page.locator("#decode-budget-override")).toBeVisible({ timeout: 5_000 });
}

async function closeSettingsModal(page: Page): Promise<void> {
  await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
  await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
}

/** Put the receiver's decode budget in Auto so the adaptive loop owns the cap. */
async function selectAutoBudget(page: Page): Promise<void> {
  await openAppearancePanel(page);
  await page.locator('[data-testid="decode-budget-auto"]').click();
  await expect(page.locator('[data-testid="decode-budget-auto"]')).toHaveAttribute(
    "aria-checked",
    "true",
  );
  await closeSettingsModal(page);
}

/**
 * Inject sustained MILD-low FPS until at least one REAL publisher is pushed to
 * the off-budget avatar tier (cap stepped down toward MIN_CAP=1). Returns once
 * an off-budget tile is present (the client has shrunk its active_decode_set so
 * the VIEWPORT excludes that publisher) or throws if the floor isn't reached
 * within the sample budget. Keeps the DOM signal and the relay signal
 * independent: this only proves the CLIENT excluded a publisher; the metric
 * proves the relay dropped its video.
 */
async function driveBudgetToFloor(page: Page): Promise<void> {
  for (let i = 0; i < MAX_DRIVE_SAMPLES; i++) {
    if ((await offBudgetTiles(page).count()) > 0) break;
    await injectFps(page, MILD_LOW_FPS);
    await page.waitForTimeout(INJECT_INTERVAL_MS);
  }
  await expect(
    offBudgetTiles(page),
    "the receiver must push at least one real publisher off-budget (active_decode_set shrank → VIEWPORT excludes it)",
  ).not.toHaveCount(0, { timeout: 15_000 });
  // MIN_CAP keeps at least one remote tile decoded. Poll (not an instantaneous
  // read) because the off-budget transition re-lays-out the grid and the
  // decoded count can momentarily read 0 mid-render before the cap re-clamps.
  await expect(decodedTiles(page), "MIN_CAP keeps one remote tile decoded").not.toHaveCount(0, {
    timeout: 15_000,
  });
}

/**
 * Poll the relay's `relay_viewport_filtered_total{room}` until it rises by at
 * least `minDelta` above `from`, or fail. This is the authoritative proof the
 * relay is DROPPING the off-screen publisher's VIDEO server-side.
 */
async function expectFilteredToClimb(
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
        latest = await readViewportFilteredTotal(transport, room);
        return latest;
      },
      { timeout: timeoutMs, intervals: [500, 1000, 2000], message },
    )
    .toBeGreaterThanOrEqual(from + minDelta);
  return latest;
}

// ---------------------------------------------------------------------------
// Parameterised relay-drop observation over BOTH transports.
// ---------------------------------------------------------------------------

for (const transport of ["websocket", "webtransport"] as const) {
  test.describe(`Viewport filter drops off-screen VIDEO over ${transport} (#995/#988)`, () => {
    // Three heavy WebCodecs renderers (1 receiver + 2 publishers). Serial +
    // generous timeout for the same renderer-footprint reason as
    // simulcast-per-receiver.spec.ts.
    test.describe.configure({ mode: "serial", timeout: 240_000 });

    test.beforeAll(async () => {
      await waitForServices();
    });

    test(`relay drops an off-screen peer's VIDEO on the ${transport} path`, async ({ baseURL }) => {
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

        const rxPage = await rxCtx.newPage();
        const pub1Page = await pub1Ctx.newPage();
        const pub2Page = await pub2Ctx.newPage();

        // Receiver is the first joiner (host) so it can disable the Waiting Room;
        // publishers join after and are auto-admitted.
        await joinMeeting(rxPage, meetingId, "VpReceiver");
        await joinMeeting(pub1Page, meetingId, "VpPublisher1");
        await joinMeeting(pub2Page, meetingId, "VpPublisher2");

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

        // Push one publisher off-screen via the adaptive decode-budget floor.
        await selectAutoBudget(rxPage);
        await driveBudgetToFloor(rxPage);

        // THE ASSERTION: the relay must now be DROPPING the off-screen
        // publisher's VIDEO on this transport's path.
        await expectFilteredToClimb(
          transport,
          meetingId,
          0,
          5,
          30_000,
          `relay_viewport_filtered_total on the ${transport} relay must climb once the receiver ` +
            "excludes a publisher — proves the transport-agnostic drop-check fires on this path",
        );
      } finally {
        await rxBrowser.close();
        await pub1Browser.close();
        await pub2Browser.close();
      }
    });
  });
}
