import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Select an option from a custom GlassSelect component.
 * Clicks the trigger button to open the dropdown menu, then clicks the
 * matching menu item.
 */
async function glassSelect(page: Page, triggerId: string, optionText: string): Promise<void> {
  const trigger = page.locator(`#${triggerId}`);
  await trigger.click();
  const menu = trigger.locator("..").locator(".glass-select-menu");
  await expect(menu).toBeVisible();
  await menu.locator(".glass-select-option", { hasText: optionText }).click();
}

/**
 * Navigate to a meeting room and join as a single user.
 *
 * Follows the same pattern used by settings-modal.spec.ts: fill meeting-id,
 * fill username, press Enter, wait for the meeting page, click the
 * "Start Meeting" / "Join Meeting" button, and wait for the grid container.
 */
async function joinMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 80 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

/**
 * Open the settings modal via the gear icon in the bottom toolbar.
 */
async function openSettingsModal(page: Page): Promise<void> {
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
}

/**
 * Switch to the Network tab inside the settings modal.
 */
async function switchToNetworkTab(page: Page): Promise<void> {
  await page.locator('[data-testid="settings-nav-network"]').click();
  await expect(page.locator(".settings-nav-button.active")).toContainText("Network");
}

/**
 * Open the diagnostics panel via the button with the "Open Diagnostics" tooltip.
 */
async function openDiagnosticsPanel(page: Page): Promise<void> {
  // The diagnostics button does not have a data-testid. Locate it via the
  // tooltip span text inside the button.
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  // Wait for the diagnostics panel to render -- it contains a section with
  // heading "Transport Preference".
  await expect(page.locator("h3", { hasText: "Transport Preference" })).toBeVisible({
    timeout: 10_000,
  });
}

/**
 * Locate the transport preference dropdown inside the diagnostics panel.
 * The dropdown is a `.peer-selector` inside the section whose h3 says
 * "Transport Preference".
 */
function diagnosticsTransportSelect(page: Page) {
  const section = page.locator(".diagnostics-section", {
    has: page.locator("h3", { hasText: "Transport Preference" }),
  });
  return section.locator(".peer-selector");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Protocol selection (transport preference)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  // 1. Settings modal shows Network tab and transport dropdown defaults to Auto
  test("settings modal Network tab shows transport dropdown defaulting to Auto", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_net_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-1");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // The transport select trigger button should be visible
    const transportSelect = page.locator("#modal-transport-select");
    await expect(transportSelect).toBeVisible();

    // Default value is "Auto"
    await expect(transportSelect.locator(".glass-select-label")).toHaveText("Auto");
  });

  // 2. Selecting a different protocol in settings shows confirm dialog
  test("changing protocol in settings shows confirm dialog", async ({ page }) => {
    const meetingId = `e2e_proto_confirm_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-2");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Set up a dialog listener to capture the confirm dialog
    let dialogMessage = "";
    page.on("dialog", async (dialog) => {
      dialogMessage = dialog.message();
      await dialog.dismiss(); // cancel to avoid reload
    });

    // Change the transport dropdown to WebSocket
    await glassSelect(page, "modal-transport-select", "WebSocket");

    // Wait a moment for the dialog event to propagate
    await page.waitForTimeout(500);

    expect(dialogMessage).toContain(
      "Changing the transport protocol will reload the page and disconnect the current call. Continue?",
    );
  });

  // 3. Cancelling the confirm dialog keeps the original value
  test("cancelling confirm dialog keeps the original Auto value", async ({ page }) => {
    const meetingId = `e2e_proto_cancel_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-3");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Dismiss (cancel) the confirm dialog
    page.on("dialog", async (dialog) => {
      await dialog.dismiss();
    });

    const transportSelect = page.locator("#modal-transport-select");
    await expect(transportSelect.locator(".glass-select-label")).toHaveText("Auto");

    // Try to change to WebTransport
    await glassSelect(page, "modal-transport-select", "WebTransport");
    await page.waitForTimeout(500);

    // The dropdown should still show "Auto" because the user cancelled
    // Note: In Dioxus, after the dialog is dismissed and the change is
    // rejected, the component re-renders with the original value.
    await expect(transportSelect.locator(".glass-select-label")).toHaveText("Auto");
  });

  // 4. Confirming the dialog saves to localStorage and reloads
  test("confirming dialog saves preference to localStorage and reloads", async ({ page }) => {
    const meetingId = `e2e_proto_save_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-4");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Accept the confirm dialog to trigger save + reload
    page.on("dialog", async (dialog) => {
      await dialog.accept();
    });

    // Change to WebSocket -- this will save and reload
    await glassSelect(page, "modal-transport-select", "WebSocket");

    // The page should reload. Wait for navigation to settle.
    // After reload, the app will load the meeting page again.
    // The localStorage value should be set before the reload happens.
    // We wait for the page to finish loading after reload.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    // Verify localStorage was set
    const storedPref = await page.evaluate(() => {
      return localStorage.getItem("vc_transport_preference");
    });
    expect(storedPref).toBe("websocket");
  });

  // 5. After reload, the saved preference persists in the dropdown
  test("saved preference persists in settings dropdown after reload", async ({ page }) => {
    const meetingId = `e2e_proto_persist_${Date.now()}`;

    // Pre-set localStorage with a transport preference before joining
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "websocket");
    });

    await joinMeeting(page, meetingId, "proto-user-5");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // The dropdown should reflect the persisted value
    const transportSelect = page.locator("#modal-transport-select");
    await expect(transportSelect.locator(".glass-select-label")).toHaveText("WebSocket");

    // Clean up: remove the localStorage entry
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
    });
  });

  // 6. Diagnostics panel shows transport preference dropdown
  test("diagnostics panel shows transport preference dropdown", async ({ page }) => {
    const meetingId = `e2e_proto_diag_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-6");

    await openDiagnosticsPanel(page);

    // The transport preference select should be visible
    const diagSelect = diagnosticsTransportSelect(page);
    await expect(diagSelect).toBeVisible();

    // Default value should be "auto" (Auto)
    await expect(diagSelect).toHaveValue("auto");
  });

  // 7. Diagnostics panel protocol change also shows confirm dialog
  test("changing protocol in diagnostics panel shows confirm dialog", async ({ page }) => {
    const meetingId = `e2e_proto_diag_confirm_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-7");

    await openDiagnosticsPanel(page);

    let dialogMessage = "";
    page.on("dialog", async (dialog) => {
      dialogMessage = dialog.message();
      await dialog.dismiss();
    });

    const diagSelect = diagnosticsTransportSelect(page);
    await diagSelect.selectOption("webtransport");
    await page.waitForTimeout(500);

    expect(dialogMessage).toContain(
      "Changing the transport protocol will reload the page and disconnect the current call. Continue?",
    );
  });

  // 8. Both dropdowns reflect the same stored preference
  test("settings modal and diagnostics panel reflect the same stored preference", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_sync_${Date.now()}`;

    // Pre-set localStorage with a specific preference
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "webtransport");
    });

    await joinMeeting(page, meetingId, "proto-user-8");

    // Check settings modal dropdown
    await openSettingsModal(page);
    await switchToNetworkTab(page);

    const settingsSelect = page.locator("#modal-transport-select");
    await expect(settingsSelect.locator(".glass-select-label")).toHaveText("WebTransport");

    // Close settings modal by clicking outside or pressing Escape
    await page.keyboard.press("Escape");
    await page.waitForTimeout(500);

    // Check diagnostics panel dropdown
    await openDiagnosticsPanel(page);

    const diagSelect = diagnosticsTransportSelect(page);
    await expect(diagSelect).toHaveValue("webtransport");

    // Clean up
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
    });
  });
});
