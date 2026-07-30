import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";

function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => {
    lines.push(msg.text());
  });
  return lines;
}

// The #1737 age-drop is a WebTransport-ONLY mechanism (it lives on the WT
// persistent-unistream send path). `createAuthenticatedContext` seeds
// `vc_transport_preference=websocket` by default, so without this the publisher
// would run on WS and `bumpStaleDeltaDrop` — a process-global counter — could
// still trip the AQ step-down log, yielding a FALSE GREEN that never exercised
// the WT path. Force sticky WebTransport before any page loads the wasm, exactly
// like wt-persistent-streams-freeze-regression.spec.ts.
async function forceWebTransportSticky(context: BrowserContext, baseURL: string): Promise<void> {
  await context.addInitScript(() => {
    try {
      localStorage.setItem("vc_transport_preference", "webtransport");
      localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the seed page below covers it */
    }
  });
  const seedPage = await context.newPage();
  await seedPage.goto(baseURL, { waitUntil: "domcontentloaded" });
  await seedPage.evaluate(() => {
    localStorage.setItem("vc_transport_preference", "webtransport");
    localStorage.setItem("vc_transport_sticky", "true");
  });
  await seedPage.close();
}

async function joinMeetingCameraOn(
  page: Page,
  meetingId: string,
  displayName: string,
): Promise<void> {
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
          /* never settled within budget; fall through without toggling */
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
                const video = el as HTMLVideoElement;
                const stream = video.srcObject as MediaStream | null;
                return stream
                  ? stream.getVideoTracks().filter((track) => track.readyState === "live").length
                  : 0;
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

// UNTAGGED (no @bvt): like the audio-uplink netsim siblings, this validates via
// `make e2e` / scoped dispatch with TRUNK_BUILD_FEATURES=netsim, not per-PR CI.
// It also runs post-merge in the full-suite `e2e-hcl.yaml` (--project=dioxus).
//
// SCOPE (stated plainly, not implied): this spec drives the SECOND half of the
// #1737 axis — counter → camera AQ step-down → production step-down log — by
// injecting the process-global stale-delta counter via the netsim hook. It does
// NOT exercise the send-path drop DECISION (`should_age_drop`, which chooses
// which stale delta frames to drop): that is bypassed by injecting the counter
// directly, and is covered instead by native unit tests
// (`should_age_drop_is_keyframe_exempt_and_fail_open`,
// `record_stale_delta_drop_increments_counter_once` in webtransport.rs). The
// user-visible outcome (a RECEIVER sees fresh-or-frozen, not minutes-behind) is
// NOT asserted here because it needs genuine WT uplink saturation to make
// `writer.ready()` slow so real frames age out — and no uplink-saturation netsim
// shaper exists today (only `downlink-impair.ts`). So this is publisher-side
// counter→AQ→log coverage; the drop decision and receiver outcome are out of
// scope for the current harness.
test("camera publisher steps video down on WT stale-delta age-drops (#1737)", async ({
  baseURL,
}) => {
  test.setTimeout(120_000);
  const uiURL = baseURL || "http://localhost:3001";
  const meetingId = `e2e_camera_age_drop_${Date.now()}`;

  // Publisher-only: the counter→AQ→step-down path under test is entirely
  // publisher-side, so a receiver would be decorative (an earlier draft joined
  // one but never asserted on it — removed to cut runtime and flake surface).
  const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
  try {
    const pubCtx = await createAuthenticatedContext(
      pubBrowser,
      "camera-age-drop-pub@videocall.rs",
      "CameraAgeDropPublisher",
      uiURL,
    );

    // #1737 is WT-only: force the PUBLISHER onto sticky WebTransport before it
    // loads the wasm, so the age-drop path under test is actually exercised.
    await forceWebTransportSticky(pubCtx, uiURL);
    // Enable the per-tile transport badge on the publisher so we can assert the
    // ELECTED transport (active_transport()) — the preference is "WT with WS
    // fallback", so the localStorage key alone cannot prove WT actually won.
    await setTransportBadgeFlag(pubCtx);

    const pubPage = await pubCtx.newPage();
    const pubConsole = collectConsole(pubPage);

    await joinMeetingCameraOn(pubPage, meetingId, "CameraAgeDropPublisher");

    // The publisher must actually be ELECTED onto WebTransport — otherwise this
    // test is a false green (the WS path never exercises the WT age-drop send
    // seam, yet bumpStaleDeltaDrop is a process-global counter that would still
    // trip the step-down log). The `vc_transport_preference` key is NOT proof:
    // it means "attempt WT with automatic WS fallback" and is never rewritten on
    // a runtime WT→WS fallback. The authoritative signal is the self-view
    // transport badge, sourced from `VideoCallClient::active_transport()` — it
    // reads "WT" only when WebTransport actually won. If WT election failed and
    // the client fell back to WS, this badge reads "WS" and the assertion FAILS
    // loud instead of silently passing on the wrong transport.
    const selfWtBadge = pubPage.locator(
      '.transport-badge[aria-label="Your connection transport: WebTransport"]',
    );
    await expect(
      selfWtBadge,
      "publisher self-view must show the ELECTED WebTransport badge (active_transport()==webtransport); " +
        "a WS badge here means WT election fell back to WebSocket and the WT-only #1737 age-drop path " +
        "was not exercised — a false green.",
    ).toHaveCount(1, { timeout: 30_000 });
    await expect(selfWtBadge).toHaveText("WT");

    const staleDeltaReady = await pubPage.evaluate(
      () =>
        typeof (window.__vcNetsim as unknown as { bumpStaleDeltaDrop?: unknown } | undefined)
          ?.bumpStaleDeltaDrop === "function",
    );
    expect(
      staleDeltaReady,
      "window.__vcNetsim.bumpStaleDeltaDrop is missing - the dioxus UI image was built " +
        "WITHOUT the `netsim` cargo feature or predates #1737. Rebuild with `make e2e-build` " +
        "(the e2e stack sets TRUNK_BUILD_FEATURES=netsim; see docker/docker-compose.e2e.yaml).",
    ).toBe(true);

    await pubPage.waitForTimeout(3000);

    const STEP_DOWN_RE =
      /CameraEncoder: client WT stale-delta backpressure detected \(\d+ camera deltas dropped in [0-9.]+ms\), forcing video step-down/;

    // MUTATION SENSITIVITY lives entirely in this poll: it bumps the stale-delta
    // counter each iteration and succeeds ONLY when the production step-down
    // warning fires. Reverting the `camera_wt_stale_drop_step_down_decision`
    // wiring in the camera monitor loop (camera_encoder.rs) makes that log never
    // fire → the poll times out at 20s → the test FAILS. No separate "guard"
    // assertion is added (an `expect(bumped > 0)` would be a tautology, reachable
    // only after the poll already resolved).
    await expect
      .poll(
        async () => {
          await pubPage.evaluate(() => {
            (
              window.__vcNetsim as unknown as {
                bumpStaleDeltaDrop?: (n: number) => unknown;
              }
            )?.bumpStaleDeltaDrop?.(15);
          });
          return pubConsole.some((line) => STEP_DOWN_RE.test(line));
        },
        {
          timeout: 20_000,
          intervals: [500, 1000],
          message:
            "expected the camera publisher to log the main-thread step-down warning " +
            "(CameraEncoder: client WT stale-delta backpressure detected ... forcing video step-down) " +
            "after sustained bumpStaleDeltaDrop(15) pressure. Its absence means the stale-delta " +
            "counter did not drive force_video_step_down through the production AQ monitor loop.",
        },
      )
      .toBe(true);
  } finally {
    await pubBrowser.close();
  }
});
