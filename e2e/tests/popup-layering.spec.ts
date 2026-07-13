import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Popup/dropdown layering and mutual exclusivity", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `popup_layer_test_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("layer-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  async function openDensityPopover(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const actionBar = page.locator(".video-controls-container");
    const trigger = actionBar.locator(
      'button:has-text("Auto"), button:has-text("Standard"), button:has-text("Dense"), button:has-text("Maximum")',
    );
    if ((await trigger.count()) > 0) {
      await trigger.first().click();
    } else {
      const fallback = actionBar.locator(
        '[class*="density"], :has-text("Auto"):not(.density-popover):not(.density-option)',
      );
      await fallback.first().click();
    }
    await expect(page.locator(".density-popover")).toBeVisible({ timeout: 5_000 });
  }

  async function openDockMenu(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const toggleBtn = page.locator('.dock-position-wrapper button[aria-haspopup="listbox"]');
    await toggleBtn.click();
    await expect(page.locator(".glass-select-menu")).toBeVisible({ timeout: 5_000 });
  }

  async function openSettings(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const settingsBtn = page.locator('[data-testid="open-settings"]');
    await settingsBtn.click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  }

  async function clickPeerListButton(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const peerBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Open Peers")') });
    await peerBtn.first().click();
  }

  async function clickDiagnosticsButton(page: Page): Promise<void> {
    await page.locator(".video-controls-container").hover();
    const diagBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Open Diagnostics")') });
    await diagBtn.first().click();
  }

  // Click the video grid on the half OPPOSITE an open side panel, in the upper
  // third so the point is well clear of the bottom action bar. The peer list
  // docks LEFT and the diagnostics drawer docks RIGHT (both float OVER the grid),
  // so clicking the far side lands on an unambiguous background/grid point — not
  // on the panel and not on the action bar. At the 1280px desktop viewport this
  // is a reliable "outside" click for the #main-container light-dismiss (#1790).
  async function clickGridClearOf(page: Page, panelSide: "left" | "right"): Promise<void> {
    const grid = page.locator("#grid-container");
    await expect(grid).toBeVisible();
    const box = await grid.boundingBox();
    if (!box) throw new Error("#grid-container has no bounding box");
    const xFrac = panelSide === "left" ? 0.75 : 0.25;
    const x = box.x + box.width * xFrac;
    const y = box.y + box.height * 0.15;
    await page.mouse.click(x, y);
  }

  test("opening dock menu closes density popover", async ({ page }) => {
    await joinMeeting(page, "dock_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await openDockMenu(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".glass-select-menu")).toBeVisible();
  });

  test("opening density popover closes dock menu", async ({ page }) => {
    await joinMeeting(page, "density_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await openDensityPopover(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".density-popover")).toBeVisible();
  });

  test("opening settings modal closes density popover", async ({ page }) => {
    await joinMeeting(page, "settings_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await openSettings(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".device-settings-modal")).toBeVisible();
  });

  test("opening settings modal closes dock menu", async ({ page }) => {
    await joinMeeting(page, "settings_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await openSettings(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator(".device-settings-modal")).toBeVisible();
  });

  test("opening peer list closes density popover", async ({ page }) => {
    await joinMeeting(page, "peers_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await clickPeerListButton(page);

    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });
  });

  test("opening diagnostics closes dock menu", async ({ page }) => {
    await joinMeeting(page, "diag_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await clickDiagnosticsButton(page);

    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });
  });

  test("clicking outside the density popover closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  test("clicking outside the dock menu closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });
  });

  test("clicking Action Bar in dock menu closes peer list", async ({ page }) => {
    await joinMeeting(page, "actionbar_closes_peerlist");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    await openDockMenu(page);
    // Dock menu opening alone must NOT close the peer list
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/);

    const actionBarOption = page.locator(".glass-select-menu .glass-select-option", {
      hasText: "Action Bar",
    });
    await actionBarOption.click();

    await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  });

  test("clicking Action Bar in dock menu closes diagnostics", async ({ page }) => {
    await joinMeeting(page, "actionbar_closes_diag");

    await clickDiagnosticsButton(page);
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });

    await openDockMenu(page);

    const actionBarOption = page.locator(".glass-select-menu .glass-select-option", {
      hasText: "Action Bar",
    });
    await actionBarOption.click();

    await expect(page.locator("#diagnostics-sidebar")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  });

  test("clicking outside the mock-peers popover closes it", async ({ page }) => {
    await joinMeeting(page, "click_outside_mock_peers");

    await page.locator(".video-controls-container").hover();
    const mockBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });

    // Skip if mock peers button doesn't exist
    if ((await mockBtn.count()) === 0) {
      test.skip();
      return;
    }

    await mockBtn.first().click();
    await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });

    await page.locator("#grid-container").click({ position: { x: 10, y: 10 } });
    await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  // ── #1790 side-panel light-dismiss (click-outside + Esc) ──
  //
  // Most of these FAIL on the pre-#1790 base: before this change the
  // `#main-container` background handler closed only the density/dock/mock
  // popovers — never the peer list or diagnostics drawer — so a grid click left
  // the panel open and Escape did nothing. Each test states its base behavior.
  //
  // The two "keep open" tests (inside-panel click, action-bar click) PASS on
  // base AND on the correct fix: their job is to catch an OVER-broad #1790 that
  // closes the panel on the wrong click (a missing container stop_propagation,
  // or a missing action-bar guard). They are stated as such in-file.
  //
  // The diagnostics "closed" assertions rely on the `#diagnostics-sidebar`
  // placeholder that persists once the drawer has been opened at least once, so
  // `not.toHaveClass(/visible/)` anchors to a real element — every test opens the
  // drawer before closing it.

  test("clicking the grid closes the peer list (#1790)", async ({ page }) => {
    await joinMeeting(page, "grid_closes_peerlist");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    // Peer list docks LEFT → click the RIGHT half of the grid.
    await clickGridClearOf(page, "left");

    // FAILS on base: no background close existed for the peer list.
    await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
  });

  test("clicking the grid closes the diagnostics drawer (#1790)", async ({ page }) => {
    await joinMeeting(page, "grid_closes_diag");

    await clickDiagnosticsButton(page);
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });

    // Diagnostics docks RIGHT → click the LEFT half of the grid.
    await clickGridClearOf(page, "right");

    // FAILS on base: no background close existed for the diagnostics drawer.
    await expect(page.locator("#diagnostics-sidebar")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
  });

  test("clicking inside the peer list keeps it open (#1790 stop-propagation)", async ({ page }) => {
    await joinMeeting(page, "inside_keeps_peerlist");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    // Click a neutral spot INSIDE the panel (its search box). This PASSES on
    // base (base never closed the panel on any click) AND on the correct fix
    // (the container stop_propagation keeps the in-panel click from reaching the
    // background handler). It FAILS on a fixed-but-broken build that dropped the
    // `#peer-list-container` stop_propagation — pinning that guard.
    await page.locator("#peer-list-container .search-input").click();

    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/);
  });

  test("clicking a mic control in the action bar keeps the peer list open (#1790)", async ({
    page,
  }) => {
    await joinMeeting(page, "actionbar_keeps_peerlist");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    // A click on an action-bar control is NOT an outside/grid click, so it must
    // leave the panel open. The mic toggle does NOT stop propagation, so this
    // relies on the click_within_action_bar guard in the background handler.
    // PASSES on base AND on the correct fix; FAILS on a #1790 that ignored the
    // action bar and closed the panel on every non-panel click.
    await page.locator(".video-controls-container").hover();
    await page.locator('[data-testid="mic-toggle-button"]').click();

    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/);
  });

  test("Escape closes the peer list and restores focus to its trigger (#1790)", async ({
    page,
  }) => {
    await joinMeeting(page, "esc_closes_peerlist");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    // Move focus INTO the panel first (empty search box) so a genuine restore is
    // observable: focus must travel from inside the panel back OUT to the
    // trigger. Empty query → Escape bubbles to the background handler.
    await page.locator("#peer-list-container .search-input").click();
    await page.keyboard.press("Escape");

    // FAILS on base: Escape did nothing for the peer list.
    await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    // Focus restored to the action-bar toggle, NOT left on <body> or the
    // now-removed search box.
    await expect
      .poll(() => page.evaluate(() => document.activeElement?.id ?? ""))
      .toBe("peer-list-trigger");
  });

  test("Escape closes the diagnostics drawer and restores focus to its trigger (#1790)", async ({
    page,
  }) => {
    await joinMeeting(page, "esc_closes_diag");

    await clickDiagnosticsButton(page);
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });

    // Focus a focusable element INSIDE the drawer (its close button) WITHOUT
    // activating it, so the Escape that follows originates from inside the panel
    // and a genuine focus restore is observable.
    await page.locator("#diagnostics-sidebar .close-button").focus();
    await page.keyboard.press("Escape");

    // FAILS on base: Escape did nothing for the diagnostics drawer.
    await expect(page.locator("#diagnostics-sidebar")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect
      .poll(() => page.evaluate(() => document.activeElement?.id ?? ""))
      .toBe("diagnostics-trigger");
  });

  test("both panels open: Escape peels diagnostics then the peer list, restoring focus each time (#1790)", async ({
    page,
  }) => {
    await joinMeeting(page, "esc_both_open_walk");

    // Open diagnostics first, then the peer list; both drawers coexist (left +
    // right) — neither open path closes the other.
    await clickDiagnosticsButton(page);
    await expect(page.locator("#diagnostics-sidebar")).toHaveClass(/visible/, { timeout: 5_000 });
    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    // First Escape → diagnostics (the topmost drawer) closes; focus lands on the
    // diagnostics trigger. FAILS on base (Escape closed neither).
    await page.locator("#peer-list-trigger").focus();
    await page.keyboard.press("Escape");
    await expect(page.locator("#diagnostics-sidebar")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/);
    await expect
      .poll(() => page.evaluate(() => document.activeElement?.id ?? ""))
      .toBe("diagnostics-trigger");

    // Second Escape → the peer list closes; focus lands on the peer-list trigger.
    // Focus moving to two DIFFERENT triggers in sequence is the strongest proof
    // that focus restore is real and per-panel, not incidental.
    await page.keyboard.press("Escape");
    await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect
      .poll(() => page.evaluate(() => document.activeElement?.id ?? ""))
      .toBe("peer-list-trigger");
  });

  test("Escape in the peer-list search clears the query first, then closes (#1790)", async ({
    page,
  }) => {
    await joinMeeting(page, "esc_search_clear_then_close");

    await clickPeerListButton(page);
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/, { timeout: 5_000 });

    const searchInput = page.locator("#peer-list-container .search-input");
    await searchInput.click();
    await searchInput.pressSequentially("abc");
    await expect(searchInput).toHaveValue("abc");

    // First Escape with a non-empty query CLEARS the field and is swallowed —
    // the panel stays open. FAILS on the unfixed input handler: without it the
    // first Escape would bubble and close the panel with the query still set.
    await page.keyboard.press("Escape");
    await expect(searchInput).toHaveValue("");
    await expect(page.locator("#peer-list-container")).toHaveClass(/visible/);

    // Second Escape (query now empty) bubbles and closes the panel, restoring
    // focus to the trigger.
    await page.keyboard.press("Escape");
    await expect(page.locator("#peer-list-container")).not.toHaveClass(/visible/, {
      timeout: 5_000,
    });
    await expect
      .poll(() => page.evaluate(() => document.activeElement?.id ?? ""))
      .toBe("peer-list-trigger");
  });

  test("pressing Escape closes density popover", async ({ page }) => {
    await joinMeeting(page, "escape_closes_density");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  // Standing coverage for the dock menu's pre-existing Escape handler
  // (attendants.rs:7627-7632). Not added by #1777 but pinned here for
  // regression safety alongside the new density/mock-peers tests.
  test("pressing Escape closes dock menu", async ({ page }) => {
    await joinMeeting(page, "escape_closes_dock");

    await openDockMenu(page);
    await expect(page.locator(".glass-select-menu")).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(page.locator(".glass-select-menu")).not.toBeVisible({ timeout: 3_000 });
  });

  test("pressing Escape closes mock-peers popover", async ({ page }) => {
    await joinMeeting(page, "escape_closes_mock_peers");

    await page.locator(".video-controls-container").hover();
    const mockBtn = page
      .locator(".video-controls-container button")
      .filter({ has: page.locator('.tooltip:has-text("Mock Peers")') });

    // Skip if mock peers button doesn't exist (env-gated to debug builds)
    if ((await mockBtn.count()) === 0) {
      test.skip();
      return;
    }

    await mockBtn.first().click();
    await expect(page.locator(".mock-peers-popover")).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 3_000 });
  });

  test("Escape-closing density popover restores focus to trigger", async ({ page }) => {
    await joinMeeting(page, "escape_restores_density_focus");

    await openDensityPopover(page);
    await expect(page.locator(".density-popover")).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(page.locator(".density-popover")).not.toBeVisible({ timeout: 3_000 });

    // Focus must land on the density trigger, not on <body>
    const trigger = page.locator("#density-mode-trigger");
    await expect(trigger).toBeFocused({ timeout: 3_000 });
  });
});
