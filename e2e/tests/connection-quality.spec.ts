import { test, expect, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import {
  authoredStrokeWidths,
  keylinesMatchTrends,
  keylinesPrecedeTrends,
  newestTrendY,
  SELF_SIGNAL_DISC,
  SELF_SIGNAL_SPARK,
  SPARK_KEYLINE,
  SPARK_MIN_POINTS,
  SPARK_REMOVED_CIRCLES,
  SPARK_TREND,
  SPARK_WARN_Y,
  sparkAreaFillState,
} from "../helpers/signal-meter";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * The self-view signal disc: resting state, placement, destination (#367, #2661).
 *
 * The NEGATIVE half. Re-pointing the pre-2661 assertions was not an option: an
 * absence check on a deleted class passes vacuously. Transitions live next door.
 */

// FOUR, not five: self moved off `Excellent`. 5 is pre-move.
const LEVEL_GOOD = "4";

const TREND_GOOD = "rgb(76, 175, 80)";

const KEYLINE_BLACK = "rgb(0, 0, 0)";

/** The probe round-trips the authored token text into a comparable colour. */
async function discAndTokenBackground(el: Locator): Promise<[string, string]> {
  return await el.evaluate((node) => {
    const probe = document.createElement("span");
    probe.style.backgroundColor = getComputedStyle(document.documentElement)
      .getPropertyValue("--overlay-heavy")
      .trim();
    document.body.appendChild(probe);
    const token = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return [getComputedStyle(node).backgroundColor, token];
  });
}

const chrome = (page: Page): Locator => page.locator(".host-tile-chrome");
const disc = (page: Page): Locator => chrome(page).locator(SELF_SIGNAL_DISC);
const spark = (page: Page): Locator => disc(page).locator(SELF_SIGNAL_SPARK);

async function joinMeeting(page: Page, label: string): Promise<void> {
  const meetingId = `e2e_cqi_${label}_${Date.now()}`;
  await fillAndSubmitJoinForm(page, meetingId, `CQI${label}User`);

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
    await joinButton
      .first()
      .click()
      .catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });

  // PRESENCE GATE: an absent cluster must name itself, not time out later.
  await expect(chrome(page)).toHaveCount(1, { timeout: 15_000 });
}

test.describe("Self signal disc (#367, #2661)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
    await enableDiagnosticsTileIndicators(context);
  });

  // TAGGED @bvt1 — the ONE per-PR executing guard for 2661; the rest is untagged.
  test("the disc is present and rests at Good on a healthy localhost link @bvt1", async ({
    page,
  }) => {
    // 120 s: the per-step budgets sum past the 60 s default, so a SLOW stack
    // would red every PR with an undiagnosable "Test timeout".
    test.setTimeout(120_000);

    // HEADLINE: nothing rendered here before 2661; restore the early return and
    // this count goes to 0.
    await joinMeeting(page, "present");

    await expect(disc(page), "the self disc must be mounted on a healthy call").toHaveCount(1, {
      timeout: 15_000,
    });
    await expect(disc(page)).toBeVisible({ timeout: 15_000 });

    await expect(disc(page)).toHaveAttribute("data-signal-state", "measured", { timeout: 20_000 });

    await expect(disc(page)).toHaveAttribute("data-signal-level", LEVEL_GOOD);
    await expect(disc(page)).toHaveAttribute("data-signal-lost", "false");
    await expect(disc(page)).toHaveAttribute(
      "aria-label",
      "Your connection: good. Open diagnostics.",
    );

    // FAILS ON 0d48bec5^: the disc wore an opaque `#101114` of its own.
    const [discBg, tokenBg] = await discAndTokenBackground(disc(page));
    expect(tokenBg, "`--overlay-heavy` must resolve, or both sides read transparent").not.toBe(
      "rgba(0, 0, 0, 0)",
    );
    expect(discBg, "the disc must wear the sibling tile icons' scrim token").toBe(tokenBg);

    await expect(spark(page)).toHaveCount(1);
    await expect
      .poll(async () => await spark(page).locator(SPARK_TREND).count(), { timeout: 20_000 })
      .toBeGreaterThanOrEqual(1);
    await expect(spark(page).locator(SPARK_TREND).first()).toHaveCSS("stroke", TREND_GOOD);

    // Discriminating: the un-fixed builder renders 3 circles here.
    await expect(
      disc(page).locator(SPARK_REMOVED_CIRCLES),
      "2661 removed the head dot AND the ring arc; neither may come back",
    ).toHaveCount(0);

    // POSITIVE: a count-0 on the deleted `.spark-grid` would pass vacuously.
    await expect
      .poll(async () => await keylinesMatchTrends(spark(page)), {
        timeout: 20_000,
        message: "every trend run rides on its own keyline, or a run loses its backdrop",
      })
      .toBe(true);
    await expect(spark(page).locator(SPARK_KEYLINE).first()).toHaveCSS("stroke", KEYLINE_BLACK);
    await expect
      .poll(async () => await keylinesPrecedeTrends(spark(page)), {
        timeout: 20_000,
        message: "every keyline must be painted UNDER the trend, not over it",
      })
      .toBe(true);
    const [keylineWidth] = await authoredStrokeWidths(spark(page).locator(SPARK_KEYLINE));
    const [trendWidth] = await authoredStrokeWidths(spark(page).locator(SPARK_TREND));
    expect(
      keylineWidth,
      `a keyline no wider than the trend it backs shows nothing (${keylineWidth} vs ${trendWidth})`,
    ).toBeGreaterThan(trendWidth);

    await expect
      .poll(async () => await sparkAreaFillState(spark(page)), {
        timeout: 20_000,
        message: "the disc interior is one flat colour: nothing may fill under the trend",
      })
      .toBe("no-area-fill");

    await expect
      .poll(async () => await newestTrendY(spark(page)), {
        timeout: 20_000,
        message: `a Good link's newest sample must plot at or above the 300 ms mark (y <= ${SPARK_WARN_Y})`,
      })
      .toBeLessThanOrEqual(SPARK_WARN_Y);
  });

  test("the connection words are gone from the self tile", async ({ page }) => {
    // LIVE DOM: a count-0 on `.connection-quality-label` would pass vacuously.
    await joinMeeting(page, "nowords");
    await expect(disc(page)).toHaveCount(1, { timeout: 15_000 });

    await expect(disc(page)).toHaveText("");

    await expect(disc(page)).toHaveAttribute("title", /^Connection: (good|measuring)/, {
      timeout: 20_000,
    });
  });

  test("the disc sits immediately inboard of the connection LED", async ({ page }) => {
    // Both clusters are `row-reverse`, so self must be LED then signal in DOM
    // order — the reverse of pre-2661, and it fails if the badge moves back.
    await joinMeeting(page, "order");
    await expect(disc(page)).toHaveCount(1, { timeout: 15_000 });

    // Presence first: a sentinel evaluate would pass on a half-built cluster.
    await expect(chrome(page).locator(".connection-led")).toHaveCount(1, { timeout: 15_000 });

    const order = await chrome(page).evaluate((el) =>
      Array.from(el.children).map((c) => ({
        tag: c.tagName,
        cls: c.className,
        testid: c.getAttribute("data-testid"),
      })),
    );
    expect(
      order.length,
      "the chrome cluster must have at least the LED and the disc",
    ).toBeGreaterThanOrEqual(2);
    expect(
      order[0].cls,
      "the connection LED is the first child (rightmost under row-reverse)",
    ).toContain("connection-led");
    expect(order[1].testid, "the signal disc is immediately inboard of the LED").toBe(
      "self-signal-indicator",
    );
  });

  test("clicking the disc opens the Diagnostics drawer", async ({ page }) => {
    // Self has no `PeerSignalHistory`, so it opens Diagnostics, not that popup.
    await joinMeeting(page, "diagopen");
    await expect(disc(page)).toHaveCount(1, { timeout: 15_000 });
    await expect(disc(page)).toBeVisible({ timeout: 15_000 });

    await expect(page.locator("#diagnostics-sidebar")).toHaveCount(0);

    await disc(page).click({ timeout: 15_000 });

    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/\bvisible\b/, {
      timeout: 15_000,
    });
    await expect(page.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 15_000 });
  });

  test("the disc does not drift off Good after an extended wait on localhost", async ({ page }) => {
    await joinMeeting(page, "extended");
    await expect(disc(page)).toHaveCount(1, { timeout: 15_000 });
    await expect(disc(page)).toHaveAttribute("data-signal-state", "measured", { timeout: 20_000 });

    await page.waitForTimeout(10_000);

    await expect(disc(page)).toHaveAttribute("data-signal-level", LEVEL_GOOD);

    await expect(spark(page)).toHaveCount(1);
    await expect
      .poll(async () => await spark(page).locator(SPARK_TREND).count(), { timeout: 15_000 })
      .toBeGreaterThanOrEqual(1);
    await expect(spark(page).locator(SPARK_TREND).first()).toHaveCSS("stroke", TREND_GOOD);

    await expect
      .poll(async () => await newestTrendY(spark(page)), {
        timeout: 15_000,
        message: "after 10 s on localhost the newest sample must still sit above the 300 ms mark",
      })
      .toBeLessThanOrEqual(SPARK_WARN_Y);

    // SPARK_MIN_POINTS not 10, for CI slack — but the product plots nothing below
    // it, so a stalled sampler still lands under.
    await expect
      .poll(async () => Number(await disc(page).getAttribute("data-signal-samples")), {
        timeout: 15_000,
      })
      .toBeGreaterThanOrEqual(SPARK_MIN_POINTS);
  });
});
