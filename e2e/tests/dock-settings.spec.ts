import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

// issue 1762: the auto-hide item's visible label is now a STABLE NOUN. It used
// to be the imperative "Turn Hiding On"/"Turn Hiding Off", which flipped with
// the state that `aria-checked` also flips — on a `menuitemcheckbox` the two
// signals then cancel and a screen reader announces the inverse of the truth
// ("Turn Hiding Off, check box, checked" while hiding is ON). Must match
// `DOCK_AUTOHIDE_LABEL` in dioxus-ui/src/components/attendants.rs.
const AUTOHIDE_LABEL = "Auto-hide action bar";

// The label no longer encodes the state, so every state check below reads
// `aria-checked` — which is the contract the fix establishes as the single
// source of truth.
function autohideItem(page: Page) {
  return page.locator(".glass-select-option").filter({ hasText: AUTOHIDE_LABEL });
}

async function autohideIsOn(page: Page): Promise<boolean> {
  return (await autohideItem(page).first().getAttribute("aria-checked")) === "true";
}

test.describe("Dock settings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    // Meeting IDs only allow ASCII alphanumerics + underscores (see
    // `is_valid_meeting_id` in videocall-types/src/validation.rs). The home
    // form's onsubmit rejects hyphens and returns early without navigating,
    // which is what previously caused all dock-settings tests to time out at
    // toHaveURL: the URL stayed at "/". Replace hyphens with underscores.
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `dock_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("dock-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Dioxus auto-joins when a display name is already set (the home form
    // sets `display_name_ctx` before navigating), so the "Start Meeting"
    // button may flash and disappear before we can click it. Race the
    // button against `#grid-container` and skip the click if the auto-join
    // has already landed us in the meeting. Mirrors the pattern PR #741
    // applied across the other 14 specs.
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      // Only click if the button is still attached — auto-join may resolve
      // between waitFor() resolving and the click landing.
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  async function openDockMenu(page: Page): Promise<void> {
    // Hover to reveal the action bar in case autohide is active
    await page.locator(".video-controls-container").hover();
    // Stable id, not `aria-haspopup` — issue 1762 changed that value from
    // "listbox" to "menu".
    const toggleBtn = page.locator("#dock-menu-trigger");
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  test("dock menu shows all entries", async ({ page }) => {
    await joinMeeting(page, "menu-entries");

    await openDockMenu(page);

    const menu = page.locator(".glass-select-menu");
    await expect(menu).toBeVisible();

    // The menu grew from 5 entries to 7 (Customize and Reset to Default were
    // added under issues 1722/1756) and the settings entry was renamed from
    // "Dock Settings" to "Action Bar…". This count is also the drift pin for
    // `DOCK_MENU_ITEM_COUNT` in attendants.rs, which drives the roving-tabindex
    // arithmetic — if an item is added without moving the constant, the roving
    // index can point at an item that does not render, and this fails first.
    const options = menu.locator(".glass-select-option");
    await expect(options).toHaveCount(7);

    await expect(options.filter({ hasText: "Bottom" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Left" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Right" })).toHaveCount(1);
    await expect(options.filter({ hasText: AUTOHIDE_LABEL })).toHaveCount(1);
    await expect(options.filter({ hasText: "Customize" })).toHaveCount(1);
    await expect(options.filter({ hasText: "Reset to Default" })).toHaveCount(1);
    // Capital "B" is load-bearing: `hasText` regexes are case-SENSITIVE, so
    // /Action Bar/ matches "Action Bar…" and NOT "Auto-hide action bar". The
    // toHaveCount(1) is what proves that separation holds.
    await expect(options.filter({ hasText: /Action Bar/ })).toHaveCount(1);

    const separators = menu.locator(".glass-select-separator");
    await expect(separators).toHaveCount(3);
  });

  test("auto-hide item announces its state, not the inverse of it", async ({ page }) => {
    // issue 1762 / WCAG 4.1.2 Name, Role, Value. The item is a
    // `menuitemcheckbox`, and the universal reading of a checkbox is "the thing
    // NAMED is true". The old imperative label flipped in lock-step with
    // `aria-checked`, so the two signals cancelled: auto-hide ON rendered
    // "Turn Hiding Off" + `aria-checked="true"`, which a screen reader speaks as
    // "Turn Hiding Off, check box, checked" — the user concludes hiding is OFF
    // at the exact moment it is ON. Both settings were misreported.
    //
    // The contract now: the NAME is invariant across both states, and
    // `aria-checked` alone carries the state. Reverting the label to the
    // imperative form fails the invariance assertion below.
    await joinMeeting(page, "autohide_name_role_value");
    await openDockMenu(page);

    const item = autohideItem(page).first();
    await expect(item).toHaveAttribute("role", "menuitemcheckbox");

    const nameWhenOff = ((await item.textContent()) ?? "").trim();
    await expect(item, "auto-hide defaults to OFF on a fresh profile").toHaveAttribute(
      "aria-checked",
      "false",
    );
    // A checked-state item must also SHOW that it is checked. `.selected` is
    // what paints the ✓ via `.glass-select-option.selected::before`; without it
    // `aria-checked="true"` shipped with no visible indicator at all — sighted
    // users saw a bare command, AT users heard a checkbox.
    await expect(item).not.toHaveClass(/\bselected\b/);

    await item.click();
    await openDockMenu(page);
    const itemOn = autohideItem(page).first();
    await expect(itemOn).toHaveAttribute("aria-checked", "true");
    await expect(
      itemOn,
      "aria-checked=true must be accompanied by the visible ✓ (.selected)",
    ).toHaveClass(/\bselected\b/);

    const nameWhenOn = ((await itemOn.textContent()) ?? "").trim();
    expect(
      nameWhenOn,
      `the accessible name must NOT flip with the state — it read "${nameWhenOff}" when ` +
        `unchecked and "${nameWhenOn}" when checked. A name that inverts alongside ` +
        `aria-checked tells a screen-reader user the opposite of the truth (WCAG 4.1.2).`,
    ).toBe(nameWhenOff);
    expect(nameWhenOn).toBe(AUTOHIDE_LABEL);
  });

  test("dock position Left changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-left");

    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Left" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });
  });

  test("dock position Right changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-right");

    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Right" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-right/, {
      timeout: 5_000,
    });
  });

  test("dock position Bottom changes action bar class", async ({ page }) => {
    await joinMeeting(page, "pos-bottom");

    // First switch to Left so we can verify switching back to Bottom
    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Left" }).click();
    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });

    // Now switch back to Bottom
    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Bottom" }).click();

    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-bottom/, {
      timeout: 5_000,
    });
  });

  test("first-time user defaults to autohide off (action bar stays visible)", async ({ page }) => {
    // Regression guard for the default-off fix: when no `vc_dock_autohide`
    // preference is persisted, the action bar must remain visible without
    // any focus or mouse interaction. We sit idle for 3s after joining and
    // confirm the controls have not picked up the `controls-hidden` class.
    await joinMeeting(page, "default_autohide_off");

    // Sanity check: no autohide preference was persisted by joinMeeting.
    const stored = await page.evaluate(() => localStorage.getItem("vc_dock_autohide"));
    expect(stored).toBeNull();

    // Move the mouse away to a neutral spot so we are not artificially
    // keeping the controls visible via hover, then sit idle.
    await page.mouse.move(0, 0);
    await page.waitForTimeout(3_000);

    await expect(page.locator(".video-controls-container")).not.toHaveClass(/controls-hidden/);

    // And the dock menu's auto-hide item must read UNCHECKED (issue 1762: the
    // label is now a stable noun, so state lives only in `aria-checked`),
    // confirming the signal initialised to false.
    await openDockMenu(page);
    await expect(autohideItem(page).first()).toHaveAttribute("aria-checked", "false");
  });

  test("unchecking auto-hide disables autohide", async ({ page }) => {
    await joinMeeting(page, "hide-off");

    // The default-off behaviour means autohide may already be off; flip it on
    // first so the subsequent "uncheck" is meaningful.
    await openDockMenu(page);
    if (!(await autohideIsOn(page))) {
      await autohideItem(page).click();
      await openDockMenu(page);
    }
    await expect(autohideItem(page).first()).toHaveAttribute("aria-checked", "true");

    await autohideItem(page).click();

    // Wait 5 seconds without mouse movement
    await page.waitForTimeout(5_000);

    // Controls should NOT be hidden
    await expect(page.locator(".video-controls-container")).not.toHaveClass(/controls-hidden/);

    // Re-open menu and verify the item now reads unchecked
    await openDockMenu(page);
    await expect(autohideItem(page).first()).toHaveAttribute("aria-checked", "false");
  });

  test("checking auto-hide re-enables autohide", async ({ page }) => {
    await joinMeeting(page, "hide-on");

    // With the default-off fix, autohide may already be disabled on first
    // load. Make sure it's off before re-enabling so this test always exercises
    // the off -> on transition.
    await openDockMenu(page);
    if (await autohideIsOn(page)) {
      await autohideItem(page).click();
      await openDockMenu(page);
    }

    // Now re-enable autohide
    await autohideItem(page).click();

    // Move mouse to trigger visibility, then move it away to a neutral spot
    await wakeControls(page);
    await page.mouse.move(0, 0);

    // Wait for the idle timeout and assert controls become hidden
    await expect(page.locator(".video-controls-container")).toHaveClass(/controls-hidden/, {
      timeout: 10_000,
    });
  });

  test("dock position persists via localStorage", async ({ page }) => {
    await joinMeeting(page, "persist_position");

    // Switch to Left
    await openDockMenu(page);
    await page.locator(".glass-select-option").filter({ hasText: "Left" }).click();
    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });

    // Verify localStorage was set
    const stored = await page.evaluate(() => localStorage.getItem("vc_dock_position"));
    expect(stored).toBe("left");

    // Reload and re-join
    await page.reload();
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {});
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    // Dock should still be on the left after reload
    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-left/, {
      timeout: 5_000,
    });
  });

  test("autohide persists via localStorage", async ({ page }) => {
    await joinMeeting(page, "persist_autohide");

    // With the default-off fix, autohide is off on first load. Flip it on
    // first so the subsequent uncheck writes an explicit `false` to
    // localStorage.
    await openDockMenu(page);
    if (!(await autohideIsOn(page))) {
      await autohideItem(page).click();
      await openDockMenu(page);
    }

    // Toggle autohide off
    await autohideItem(page).click();

    // Verify localStorage
    const stored = await page.evaluate(() => localStorage.getItem("vc_dock_autohide"));
    expect(stored).toBe("false");

    // Reload and re-join
    await page.reload();
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {});
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });

    // Wait without mouse movement — controls should stay visible (autohide off)
    await page.waitForTimeout(5_000);
    await expect(page.locator(".video-controls-container")).not.toHaveClass(/controls-hidden/);
  });

  test("Appearance panel dock position syncs with action bar", async ({ page }) => {
    await joinMeeting(page, "appearance_dock_sync");

    // Open Settings → Preferences. The menu ITEM is "Action Bar…" (renamed from
    // "Dock Settings", a string that no longer appears anywhere in
    // dioxus-ui/src), and the settings it opens moved from the Appearance panel
    // to Preferences.
    await openDockMenu(page);
    await page
      .locator(".glass-select-option")
      .filter({ hasText: /Action Bar/ })
      .click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    // Preferences, not Appearance: `preferences_settings_panel.rs` now owns the
    // "Action Bar" section, and the menu item sets the initial section to
    // "preferences" to match.
    await expect(page.locator(".settings-nav-button.active")).toContainText("Preferences");

    // Click Right in the Position segmented control
    const posGroup = page.locator(
      '#settings-panel-preferences .transport-segmented[role="radiogroup"][aria-label="Action bar position"]',
    );
    await posGroup.locator('button[role="radio"]').filter({ hasText: "Right" }).click();

    // Verify Right is selected
    await expect(
      posGroup.locator('button[role="radio"].selected').filter({ hasText: "Right" }),
    ).toBeVisible({ timeout: 5_000 });

    // Close the modal
    await page.locator('.device-settings-modal button[aria-label="Close settings"]').click();
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });

    // Action bar should now be dock-right
    await expect(page.locator(".video-controls-container")).toHaveClass(/dock-right/, {
      timeout: 5_000,
    });
  });

  test("Action Bar… opens Preferences tab in settings modal", async ({ page }) => {
    await joinMeeting(page, "dock-settings-modal");

    await openDockMenu(page);
    await page
      .locator(".glass-select-option")
      .filter({ hasText: /Action Bar/ })
      .click();

    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

    // Verify the active tab is Preferences. The menu item sets
    // `device_settings_initial_section` to "preferences"; the old "Appearance"
    // expectation dated from before the Action Bar section moved panels.
    await expect(page.locator(".settings-nav-button.active")).toContainText("Preferences");
    await expect(page.locator("#settings-panel-preferences")).toBeVisible();

    // Verify the "Action Bar" section heading is visible inside that panel —
    // i.e. the entry point actually lands on the settings it names. The section
    // used to be headed "Dock Settings" in the Appearance panel.
    await expect(
      page.locator("#settings-panel-preferences .appearance-section-title").filter({
        hasText: "Action Bar",
      }),
    ).toBeVisible();
  });
});
