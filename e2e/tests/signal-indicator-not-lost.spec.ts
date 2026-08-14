import { test, expect, Page, chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Per-tile SIGNAL INDICATOR trust check (issue #2190).
 *
 * WHY ITS OWN FILE — and a correction to my first explanation. `playwright.config.ts` sets
 * `workers: 2` with `fullyParallel: false`, which means tests within ONE file run serially in
 * one worker while FILES are distributed across the two workers. So my original rationale
 * ("same-file tests run concurrently") was backwards, and "separate file => separate worker
 * slot" buys no isolation — if anything, splitting makes concurrent scheduling with the
 * sibling possible where co-location made it impossible.
 *
 * What I actually measured: co-located, this test's 2 browsers plus
 * `tile-order-camera-toggle`'s 3 made the pre-existing sibling fail on a 20 s locator wait;
 * split out, both pass (sibling ~45 s, this ~30 s). The flake is real but its cause is not
 * the config — most likely total browser/fake-media pressure within one 180 s test timeout.
 * Keeping the split because it is empirically green, not because of a parallelism theory.
 *
 * WHAT IT GUARDS. The tagged test pins the normal measured state on a real 2-peer call.
 * The off-budget reproduction below is intentionally untagged and marked `fixme` because
 * #2193 prevents the docker/CI stack from materialising the remote canvases that drive budget
 * shedding. The exact neutral `Unmeasured` render state is therefore guarded today by the
 * production-path component test in `peer_tile.rs`; the E2E remains ready to enable when #2193
 * restores its precondition.
 */

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
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
  if (guestResult !== "waiting") return;

  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
  await hostPage.waitForTimeout(3000);

  const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
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

test.describe("Per-tile signal indicator", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Issue #2190: the per-tile SIGNAL INDICATOR must not badge a healthy peer as
   * "connection lost".
   *
   * Why this exists. #2190 made `fps_received` count DECODED frames instead of arrivals,
   * which is correct — but `video_quality` is `fps / 30` and it drives a rendered TRUST
   * indicator (`data-signal-level` bar count + `data-signal-lost`, the red diagonal slash).
   * Before the fix, wrong-rung simulcast arrivals kept that counter warm, so the video term
   * was pinned at 1.0 and the indicator could not fall. After it, any state where this client
   * stops decoding a peer drops the term to 0.0 — and a MUTED off-budget peer scored exactly
   * 0.0, which `SignalLevel::from_quality` maps to `Lost`. The user would see a peer whose
   * connection is fine, on a tile THEIR OWN client chose not to decode, badged as
   * disconnected.
   *
   * `data-signal-level` / `data-signal-lost` were asserted NOWHERE in e2e/ before this
   * (verified by grep), so the whole indicator was uncovered — which is why the regression
   * was invisible to the suite. It is a general guard on the indicator — NOT, on its own,
   * a reproduction of the off-budget case (see the scope note at the top of the file).
   *
   * Tagged @bvt1 so it runs in per-PR CI.
   */
  test("healthy camera-on peers are not badged as signal-lost @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `signal_not_lost_${Date.now()}`;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserA = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "signalhost@videocall.rs",
      "SignalHost",
      uiURL,
    );
    await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const guestCtx = await createAuthenticatedContext(
      browserA,
      "signalguest@videocall.rs",
      "SignalGuest",
      uiURL,
    );
    await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

    const hostPage = await hostCtx.newPage();
    const guestPage = await guestCtx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "SignalHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(guestPage, meetingId, "SignalGuest");
      const guestResult = await joinMeetingFromPage(guestPage);
      await admitGuestIfNeeded(hostPage, guestPage, guestResult);

      // The guest's tile must exist on the host grid before its indicator means anything.
      const guestTile = hostPage.locator("#grid-container .grid-item", {
        has: hostPage.locator(`text="SignalGuest"`),
      });
      await expect(guestTile).toBeVisible({ timeout: 45_000 });

      // The signal indicator renders unconditionally on a peer tile (canvas or avatar).
      const signalIcon = guestTile.locator("[data-signal-level]").first();
      await expect(signalIcon).toHaveCount(1, { timeout: 30_000 });

      // Give the 1 Hz signal sampler several windows to publish real samples — the meter
      // starts empty and fills from diagnostics, so asserting immediately would read a
      // startup transient rather than steady state.
      await hostPage.waitForTimeout(6000);

      // PRECONDITION on the sampler, not on a canvas. Tony asked for a camera-published
      // guard; a `canvas` assertion is the wrong one HERE because remote decoding canvases do
      // not materialise on this docker stack (#2193 — verified: the pre-existing
      // `decode-budget-play-button.spec.ts` fails on its own locator for the same reason), so
      // it would make this spec red for an environment problem rather than a regression.
      //
      // Assert the rendered indicator contract. This does not prove a decoded canvas exists;
      // that stronger precondition is blocked by #2193 and belongs with its fix.
      await expect
        .poll(async () => await signalIcon.getAttribute("data-signal-level"), {
          timeout: 30_000,
          message: "the signal meter must render a level attribute before it can be asserted",
        })
        .not.toBeNull();

      // A camera-on, mic-on peer on a healthy local call must never be badged as
      // disconnected. `data-signal-lost` is exactly what paints the red diagonal slash.
      // (General guard on the indicator; see the scope note at the top of the file.)
      await expect
        .poll(async () => await signalIcon.getAttribute("data-signal-lost"), {
          timeout: 30_000,
          message:
            "a healthy camera-on peer must never render data-signal-lost=true — that is the " +
            "red 'connection lost' slash, and #2190 made it reachable for peers this client " +
            "merely stopped decoding",
        })
        .toBe("false");

      // Bar count, with a DISCRIMINATING threshold. `bars() > 0` was entailed by the poll
      // above (`bars() == 0` <=> `Lost` <=> `is_lost()`), so it proved nothing extra. `>= 4`
      // separates a healthy read from the pre-fix off-budget arithmetic `(1.0 + 0)/2 = 0.5`
      // => `Fair` => 3 bars.
      const bars = Number(await signalIcon.getAttribute("data-signal-level"));
      expect(
        bars,
        `a healthy decoding peer must read >= 4 bars; 3 is the pre-fix averaged-in-zero value ` +
          `(got ${bars})`,
      ).toBeGreaterThanOrEqual(4);
    } finally {
      await browserHost.close();
      await browserA.close();
    }
  });

  /**
   * Issue #2190 B2 REPRODUCTION: an OFF-BUDGET peer must not be badged "connection lost".
   *
   * This is the case the sibling test above cannot reach, and the one the fix exists for. It
   * uses the harness that already exists rather than deferring: `vc_decode_budget_override` is
   * a production-read localStorage key, and `decode-budget-play-button.spec.ts` established
   * this exact shape — pin the host to a fixed cap, join more camera-on guests than the cap,
   * and the surplus deterministically render as `.off-budget-tile`.
   *
   * My earlier deferral claimed this needed 7+ browsers against `IOS_DECODE_TILE_CEILING_ABS`
   * (6). That was wrong twice over: the iOS ceiling returns `None` on desktop Chrome so it
   * never binds at all, and the override makes the cap arbitrary. Corrected after review.
   *
   * The expected state is neutral and explicit: zero filled bars, no red lost slash, and
   * `data-signal-state="unmeasured"`.
   */
  test("off-budget peers render signal as unmeasured (pending #2193)", async ({ baseURL }) => {
    test.setTimeout(180_000);
    test.fixme(
      true,
      "blocked by #2193: docker/CI does not materialise remote canvases, so decode-budget " +
        "shedding never reaches the off-budget render path",
    );
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `signal_offbudget_${Date.now()}`;
    // Cap of 1 decoded tile: with 2 camera-on guests, exactly one is shed.
    const FORCED_BUDGET = 1;

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserA = await chromium.launch({ args: BROWSER_ARGS });
    const browserB = await chromium.launch({ args: BROWSER_ARGS });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "obhost@videocall.rs",
      "ObHost",
      uiURL,
    );
    await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
    // Pin the budget BEFORE navigation so the shed is deterministic rather than dependent on
    // auto-adaptation timing.
    await hostCtx.addInitScript(
      `localStorage.setItem("vc_decode_budget_override", "${FORCED_BUDGET}");`,
    );

    const mk = async (browser: typeof browserA, email: string, name: string) => {
      const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
      await ctx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      return ctx;
    };
    const guestACtx = await mk(browserA, "obguesta@videocall.rs", "ObGuestA");
    const guestBCtx = await mk(browserB, "obguestb@videocall.rs", "ObGuestB");

    const hostPage = await hostCtx.newPage();
    const guestAPage = await guestACtx.newPage();
    const guestBPage = await guestBCtx.newPage();

    try {
      await navigateToMeeting(hostPage, meetingId, "ObHost");
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await navigateToMeeting(guestAPage, meetingId, "ObGuestA");
      await admitGuestIfNeeded(hostPage, guestAPage, await joinMeetingFromPage(guestAPage));

      await navigateToMeeting(guestBPage, meetingId, "ObGuestB");
      await admitGuestIfNeeded(hostPage, guestBPage, await joinMeetingFromPage(guestBPage));

      // Both guest tiles present on the host grid.
      await expect(hostPage.locator("#grid-container .grid-item")).toHaveCount(2, {
        timeout: 60_000,
      });

      // PRECONDITION: the budget actually shed a tile. Without this the test would silently
      // assert nothing about the off-budget path.
      //
      // Once #2193 is fixed, this precondition must fail loudly if the budget does not shed.
      const offBudget = hostPage.locator("#grid-container .grid-item.off-budget-tile");
      await expect(offBudget).toHaveCount(1, { timeout: 30_000 });

      // Let the 1 Hz sampler publish real samples for the parked peer.
      await hostPage.waitForTimeout(6000);

      const icon = offBudget.locator("[data-signal-level]").first();
      await expect(icon).toHaveCount(1, { timeout: 30_000 });

      await expect(icon).toHaveAttribute("data-signal-state", "unmeasured");
      await expect(icon).toHaveAttribute("data-signal-lost", "false");
      await expect(icon).toHaveAttribute("data-signal-level", "0");
      await expect(icon).toHaveAttribute(
        "aria-label",
        "Video paused to save CPU. Signal is not measured for this peer.",
      );
    } finally {
      await browserHost.close();
      await browserA.close();
      await browserB.close();
    }
  });
});
