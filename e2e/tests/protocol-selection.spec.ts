import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

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

  // 1. Network tab shows segmented control with Auto selected by default
  test("Network tab shows segmented control with Auto selected by default", async ({ page }) => {
    const meetingId = `e2e_proto_default_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-1");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Auto pill should be selected by default
    await expect(page.locator('[data-testid="transport-radio-auto"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expect(page.locator('[data-testid="transport-radio-webtransport"]')).toHaveAttribute(
      "aria-checked",
      "false",
    );
    await expect(page.locator('[data-testid="transport-radio-websocket"]')).toHaveAttribute(
      "aria-checked",
      "false",
    );

    // Apply button should not be visible (no pending change)
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();

    // Sticky toggle row should not be visible while Auto is selected
    await expect(page.locator("#sticky-transport-checkbox")).not.toBeVisible();
  });

  // 2. Selecting WebSocket shows Apply button and sticky toggle
  test("selecting WebSocket shows Apply button and sticky toggle", async ({ page }) => {
    const meetingId = `e2e_proto_select_ws_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-2");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    await page.locator('[data-testid="transport-radio-websocket"]').click();

    await expect(page.locator('[data-testid="transport-radio-websocket"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();
    await expect(page.locator("#sticky-transport-checkbox")).toBeVisible();
  });

  // 3. Selecting Auto hides Apply button when already on Auto
  test("selecting Auto hides Apply button when already on Auto", async ({ page }) => {
    const meetingId = `e2e_proto_auto_hide_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-3");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Initially Auto is selected, no pending change -> Apply hidden
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();

    // Pick WebSocket -> Apply appears
    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();

    // Pick Auto again -> Apply disappears (matches current active)
    await page.locator('[data-testid="transport-radio-auto"]').click();
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();
  });

  // 4. Sticky toggle not visible when Auto is selected
  test("sticky toggle not visible when Auto is selected", async ({ page }) => {
    const meetingId = `e2e_proto_sticky_hidden_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-4");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Auto should already be selected; clicking it again should be a no-op.
    await page.locator('[data-testid="transport-radio-auto"]').click();

    await expect(page.locator("#sticky-transport-checkbox")).not.toBeVisible();
  });

  // 5. Apply without sticky writes to sessionStorage, not localStorage
  test("Apply without sticky writes to sessionStorage not localStorage", async ({ page }) => {
    const meetingId = `e2e_proto_session_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-5");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();

    await page.locator('[data-testid="transport-apply-button"]').click();

    // Apply triggers a reload -- wait for it to settle.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    const storage = await page.evaluate(() => ({
      session: sessionStorage.getItem("vc_transport_session"),
      preference: localStorage.getItem("vc_transport_preference"),
      sticky: localStorage.getItem("vc_transport_sticky"),
    }));

    expect(storage.session).toBe("websocket");
    expect(storage.preference).toBeNull();
    expect(storage.sticky).toBeNull();

    // Clean up the session-scoped key so the next test isn't affected.
    await page.evaluate(() => {
      sessionStorage.removeItem("vc_transport_session");
    });
  });

  // 6. Sticky toggle immediately writes to localStorage on check
  test("sticky toggle immediately writes to localStorage on check", async ({ page }) => {
    const meetingId = `e2e_proto_sticky_check_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-6");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await expect(page.locator("#sticky-transport-checkbox")).toBeVisible();

    await page.locator("#sticky-transport-checkbox").check();

    // Give Dioxus a moment to flush the side-effect to storage.
    await expect
      .poll(
        async () =>
          await page.evaluate(() => ({
            preference: localStorage.getItem("vc_transport_preference"),
            sticky: localStorage.getItem("vc_transport_sticky"),
          })),
        { timeout: 5000 },
      )
      .toEqual({ preference: "websocket", sticky: "true" });

    // Clean up so subsequent tests start fresh.
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
      localStorage.removeItem("vc_transport_sticky");
    });
  });

  // 7. Sticky toggle immediately clears localStorage on uncheck
  test("sticky toggle immediately clears localStorage on uncheck", async ({ page }) => {
    const meetingId = `e2e_proto_sticky_uncheck_${Date.now()}`;

    // Pre-set sticky preference before joining
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "websocket");
      localStorage.setItem("vc_transport_sticky", "true");
    });
    await page.reload();

    await joinMeeting(page, meetingId, "proto-user-7");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Sticky checkbox should be checked because storage flagged it.
    const stickyCheckbox = page.locator("#sticky-transport-checkbox");
    await expect(stickyCheckbox).toBeVisible();
    await expect(stickyCheckbox).toBeChecked();

    await stickyCheckbox.uncheck();

    await expect
      .poll(
        async () =>
          await page.evaluate(() => ({
            preference: localStorage.getItem("vc_transport_preference"),
            sticky: localStorage.getItem("vc_transport_sticky"),
          })),
        { timeout: 5000 },
      )
      .toEqual({ preference: null, sticky: null });
  });

  // 8. Apply with sticky writes to localStorage and survives reload
  test("Apply with sticky writes to localStorage and survives reload", async ({ page }) => {
    const meetingId = `e2e_proto_apply_sticky_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-8");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await page.locator("#sticky-transport-checkbox").check();

    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();
    await page.locator('[data-testid="transport-apply-button"]').click();

    // Apply triggers a reload.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    const storage = await page.evaluate(() => ({
      preference: localStorage.getItem("vc_transport_preference"),
      sticky: localStorage.getItem("vc_transport_sticky"),
    }));

    expect(storage.preference).toBe("websocket");
    expect(storage.sticky).toBe("true");

    // Clean up so subsequent tests aren't polluted.
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
      localStorage.removeItem("vc_transport_sticky");
    });
  });

  // 9. Selecting Auto and applying clears all storage
  test("selecting Auto and applying clears all storage", async ({ page }) => {
    const meetingId = `e2e_proto_auto_clear_${Date.now()}`;

    // Pre-seed all three keys so the page boots into the websocket transport.
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "websocket");
      localStorage.setItem("vc_transport_sticky", "true");
      sessionStorage.setItem("vc_transport_session", "websocket");
    });
    await page.reload();

    await joinMeeting(page, meetingId, "proto-user-9");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // The current active protocol is websocket. To make Apply appear after
    // selecting Auto we first nudge the pending selection to websocket so
    // the picker has a clean state, then switch to Auto -> Apply appears.
    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await page.locator('[data-testid="transport-radio-auto"]').click();

    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();
    await page.locator('[data-testid="transport-apply-button"]').click();

    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    const storage = await page.evaluate(() => ({
      preference: localStorage.getItem("vc_transport_preference"),
      sticky: localStorage.getItem("vc_transport_sticky"),
      session: sessionStorage.getItem("vc_transport_session"),
    }));

    expect(storage.preference).toBeNull();
    expect(storage.sticky).toBeNull();
    expect(storage.session).toBeNull();
  });

  // 10. Diagnostics panel shows transport preference dropdown
  test("diagnostics panel shows transport preference dropdown", async ({ page }) => {
    const meetingId = `e2e_proto_diag_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-10");

    await openDiagnosticsPanel(page);

    // The transport preference select should be visible
    const diagSelect = diagnosticsTransportSelect(page);
    await expect(diagSelect).toBeVisible();

    // Default value should be "auto" (Auto)
    await expect(diagSelect).toHaveValue("auto");
  });

  // 11. Diagnostics panel protocol change still shows confirm dialog
  test("changing protocol in diagnostics panel shows confirm dialog", async ({ page }) => {
    const meetingId = `e2e_proto_diag_confirm_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-11");

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

  // 12. Both surfaces reflect the same stored sticky preference
  test("settings modal and diagnostics panel reflect the same stored sticky preference", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_sync_${Date.now()}`;

    // Pre-set sticky localStorage with a specific preference
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "webtransport");
      localStorage.setItem("vc_transport_sticky", "true");
    });
    await page.reload();

    await joinMeeting(page, meetingId, "proto-user-12");

    // Check settings modal segmented control
    await openSettingsModal(page);
    await switchToNetworkTab(page);

    await expect(page.locator('[data-testid="transport-radio-webtransport"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );

    // Close settings modal
    await page.keyboard.press("Escape");
    await page.waitForTimeout(500);

    // Check diagnostics panel dropdown
    await openDiagnosticsPanel(page);

    const diagSelect = diagnosticsTransportSelect(page);
    await expect(diagSelect).toHaveValue("webtransport");

    // Clean up both keys
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
      localStorage.removeItem("vc_transport_sticky");
    });
  });
});
