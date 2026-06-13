import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Regression test for #1116: a low-core / "older Intel Mac" device must NEVER be
 * gated out of joining a meeting by a pre-join CPU-capability check.
 *
 * PR #1112 removed the pre-join capability gate entirely (per product decision
 * #1054 — every client joins regardless of device facts). Specifically it
 * deleted:
 *
 *   - The "Device not supported" hard-block card, shown when
 *     navigator.hardwareConcurrency was < 4 OR unavailable (reported as 0). It
 *     replaced the Start/Join button with a dead-end block — no way to enter the
 *     meeting.
 *   - The "Performance warning" modal (role="dialog", aria-labelledby=
 *     "capability-warn-title", title "Performance warning") shown for 4-6 cores
 *     and older Intel Macs (macOS 14, or macOS 15 with <= 8 cores). It interposed
 *     a "Switch to audio-only" / "Continue anyway" choice between the user and
 *     the join action.
 *
 * Nothing else in the suite forces a low-core / older-Intel profile, so a
 * regression that re-introduced ANY core-count or UA gate (a block card, or a
 * disabled join button on a weak profile) would pass CI silently. This spec
 * closes that gap: it spoofs a marginal device BEFORE the wasm boots and proves
 * the join button is present, enabled, free of any block/warning, and that
 * clicking it actually completes the join.
 *
 * The pure-logic ceiling derivation that #1112 kept lives in
 * dioxus-ui/src/components/capability_check.rs — note its module docs:
 * "It is not a join gate ... The old assess_from_inputs / assess_capability
 * verdict path that gated join has been removed."
 *
 * CRITICAL CAVEAT (do not break): lowering navigator.hardwareConcurrency ALSO
 * lowers the retained simulcast *ceiling* to 1 layer (capability_check.rs:
 * `cores < 6` is marginal -> 1 layer; `cores == 0` likewise). That is expected
 * and correct. This spec MUST NOT assert anything about simulcast layer count,
 * or it would couple to the ceiling and flake. We assert ONLY that join is not
 * gated.
 */

// A macOS 14 Intel User-Agent. Per capability_check.rs::is_older_intel_mac,
// any "macOS 14*" host trips the older-Intel rule unconditionally (regardless of
// core count) — this was exactly the profile the removed "Performance warning"
// modal targeted. Using a real-shaped Chrome-on-Intel-Mac UA so
// parse_platform_from_ua() extracts "macOS 14" from the "Mac OS X 14_5_0" token.
const OLDER_INTEL_MAC_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5_0) AppleWebKit/537.36 " +
  "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/**
 * Locator for the removed "Performance warning" capability modal. Built as a
 * role=dialog scoped to the modal's title text, so it matches the OLD DOM:
 * `<div role="dialog" aria-labelledby="capability-warn-title"> ...
 * <h_ id="capability-warn-title">Performance warning</h_> ...`. If that modal
 * were re-introduced this locator resolves to it; against current always-join
 * behaviour it resolves to nothing (count 0).
 */
function capabilityWarnModal(page: Page) {
  return page.getByRole("dialog").filter({ hasText: /Performance warning/i });
}

/**
 * Seed a display name BEFORE navigation so the pre-join LOBBY (PreJoinSettingsCard,
 * where the removed capability gate lived) renders. The e2e stack runs with
 * oauth disabled (scripts/config.js `oauthEnabled: "false"`), and the display
 * name is only derived from the session cookie when oauth is ON
 * (meeting.rs gates that on `oauth_enabled()`). Without a seeded name,
 * `maybe_username` is None and MeetingPage renders the "Enter your display name"
 * FORM instead — and that form's own submit button is ALSO labelled
 * "Join Meeting" (meeting.rs:716), so the join-button locator would match the
 * wrong screen, clicking it would only save the name (never join), and the
 * post-click "Your meeting is ready!" assertion would time out. Seeding the name
 * (exactly as prejoin-no-auto-start.spec.ts does) puts us on the real lobby;
 * direct nav is not "from waiting room" so auto-join stays off and the join
 * button is presented for us to click.
 */
async function seedDisplayName(page: Page): Promise<void> {
  await page.addInitScript(() => {
    try {
      localStorage.setItem("vc_display_name", "CapTestUser");
    } catch {
      // localStorage may be unavailable this early in some engines; best effort.
    }
  });
}

/**
 * Assert the page is on the pre-join screen with a fully usable join button and
 * NONE of the removed capability-gate UI present. Shared by every case so a
 * regression in any one profile fails identically.
 *
 * The button must be both VISIBLE and ENABLED — asserting only visibility would
 * let a "disabled join button on a weak device" regression slip through, which
 * is one of the exact gate shapes #1116 must catch.
 */
async function expectJoinNotGated(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", {
    name: /Start Meeting|Join Meeting/,
  });

  // The button must appear (pre-join screen rendered, not replaced by a block
  // card) within the wasm-boot budget the other pre-join specs use.
  await joinButton.waitFor({ timeout: 30_000 });

  // VISIBLE + ENABLED: a re-introduced gate that disables the button (rather
  // than hiding it) is still a regression, so both must hold.
  await expect(joinButton).toBeVisible();
  await expect(joinButton).toBeEnabled();

  // The removed "Device not supported" hard-block card must be absent. Asserting
  // count 0 (not just not-visible) keeps this robust now that the text is fully
  // gone, and catches the block re-appearing anywhere in the DOM.
  await expect(page.getByText(/Device not supported/i)).toHaveCount(0);

  // The removed "Performance warning" role=dialog modal must be absent. Scoped
  // to the modal title so it cannot accidentally match an unrelated dialog.
  await expect(capabilityWarnModal(page)).toHaveCount(0);

  // Belt-and-suspenders: the modal's action buttons must also be absent. If the
  // modal regressed under a different container/role, these still catch it.
  await expect(page.getByRole("button", { name: /Switch to audio-only/i })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /Continue anyway/i })).toHaveCount(0);

  // Pre-join: the in-meeting empty-state ("Your meeting is ready!") must NOT be
  // showing yet — proves we assert the gate-absence on the PRE-join screen, and
  // gives the post-click transition a real before/after to verify.
  await expect(page.getByText("Your meeting is ready!")).not.toBeVisible();

  // Join must actually complete when clicked — the whole point of "not gated".
  // A regression that lets the button render enabled but blocks the click path
  // (e.g. an onclick guard) would be caught here.
  await joinButton.click();
  await expect(page.getByText("Your meeting is ready!")).toBeVisible({
    timeout: 30_000,
  });
}

test.describe("Pre-join is never gated by a device-capability block (#1116)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  // Case A — low core count. hardwareConcurrency = 2 was below the removed hard-
  // block floor (cores < 4 -> "Device not supported"). Must still join freely.
  test("low-core device (hardwareConcurrency=2) is not blocked from joining @bvt1", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      try {
        Object.defineProperty(navigator, "hardwareConcurrency", {
          configurable: true,
          get: () => 2,
        });
      } catch {
        // If the property is non-configurable in some engine, fall through —
        // the real CI runner already reports a low core count, so the spec is
        // still exercising a marginal profile.
      }
    });

    await seedDisplayName(page);
    const meetingId = `e2e_low_core_${Date.now()}`;
    await page.goto(`/meeting/${meetingId}`);

    await expectJoinNotGated(page);
  });

  // Case A' — unknown core count. hardwareConcurrency = 0 reproduces the old
  // "unknown -> block" branch (navigator.hardwareConcurrency unavailable was
  // treated as a hard block by the removed gate; capability_check.rs now maps
  // 0 to the marginal/1-layer ceiling with NO block).
  test("unknown-core device (hardwareConcurrency=0) is not blocked from joining @bvt1", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      try {
        Object.defineProperty(navigator, "hardwareConcurrency", {
          configurable: true,
          get: () => 0,
        });
      } catch {
        // See Case A note.
      }
    });

    await seedDisplayName(page);
    const meetingId = `e2e_unknown_core_${Date.now()}`;
    await page.goto(`/meeting/${meetingId}`);

    await expectJoinNotGated(page);
  });

  // Case B — older Intel Mac (macOS 14) User-Agent. This profile tripped the
  // removed "Performance warning" modal regardless of core count. A per-test
  // context lets us spoof the UA at the browser level (cleaner than patching
  // navigator.userAgent after load), then we inject the session cookie into that
  // context exactly as beforeEach does for the default one.
  test("older Intel Mac UA (macOS 14) is not warned/blocked from joining @bvt1", async ({
    browser,
    baseURL,
  }) => {
    const context = await browser.newContext({
      baseURL,
      ignoreHTTPSErrors: true,
      userAgent: OLDER_INTEL_MAC_UA,
    });
    try {
      await injectSessionCookie(context, { baseURL });
      const page = await context.newPage();
      await seedDisplayName(page);

      const meetingId = `e2e_older_intel_${Date.now()}`;
      await page.goto(`/meeting/${meetingId}`);

      await expectJoinNotGated(page);
    } finally {
      await context.close();
    }
  });
});
