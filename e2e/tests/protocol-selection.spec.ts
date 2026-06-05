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

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
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

  // 1. Network tab shows segmented control with WebTransport selected by default
  test("Network tab shows only WebTransport and WebSocket with WebTransport selected by default", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_default_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-1");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // WebTransport pill should be selected by default (the new default — was Auto)
    await expect(page.locator('[data-testid="transport-radio-webtransport"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expect(page.locator('[data-testid="transport-radio-websocket"]')).toHaveAttribute(
      "aria-checked",
      "false",
    );

    // Auto option must no longer exist — the simplification removed it
    await expect(page.locator('[data-testid="transport-radio-auto"]')).toHaveCount(0);

    // The radiogroup must expose exactly two pills (WebTransport + WebSocket).
    await expect(page.locator('.transport-segmented [role="radio"]')).toHaveCount(2);

    // Apply button should not be visible (no pending change)
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();

    // Sticky toggle row should not be visible while the default is selected
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

  // 3. Selecting back to default WebTransport hides Apply button
  test("selecting WebTransport (default) hides Apply button when matching active", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_default_hide_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-3");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Initially WebTransport (default) is selected, no pending change -> Apply hidden
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();

    // Pick WebSocket -> Apply appears
    await page.locator('[data-testid="transport-radio-websocket"]').click();
    await expect(page.locator('[data-testid="transport-apply-button"]')).toBeVisible();

    // Pick WebTransport again -> Apply disappears (matches current active default)
    await page.locator('[data-testid="transport-radio-webtransport"]').click();
    await expect(page.locator('[data-testid="transport-apply-button"]')).not.toBeVisible();
  });

  // 4. Sticky toggle not visible when default (WebTransport) is selected
  test("sticky toggle not visible when WebTransport (default) is selected", async ({ page }) => {
    const meetingId = `e2e_proto_sticky_hidden_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-4");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // WebTransport (default) should already be selected; clicking it again is a no-op.
    await page.locator('[data-testid="transport-radio-webtransport"]').click();

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

    await page.locator("#sticky-transport-checkbox").check({ force: true });

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

    await stickyCheckbox.uncheck({ force: true });

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
    await page.locator("#sticky-transport-checkbox").check({ force: true });

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

  // 9. Selecting WebTransport (default) without sticky and applying clears all storage
  test("selecting WebTransport (default) without sticky and applying clears all storage", async ({
    page,
  }) => {
    const meetingId = `e2e_proto_default_clear_${Date.now()}`;

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

    // Active protocol is websocket. We need Apply to appear when we switch
    // back to the default — first un-tick sticky so the apply logic clears
    // storage instead of writing a fresh sticky=true.
    await page.locator("#sticky-transport-checkbox").uncheck({ force: true });
    await page.locator('[data-testid="transport-radio-webtransport"]').click();

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

  // 9b. Legacy "auto" persisted value migrates to WebTransport on load
  test('legacy persisted "auto" value migrates to WebTransport on load', async ({ page }) => {
    const meetingId = `e2e_proto_legacy_auto_${Date.now()}`;

    // Plant the legacy sticky+auto pair an older release would have written.
    await page.goto("/");
    await page.waitForTimeout(1500);
    await page.evaluate(() => {
      localStorage.setItem("vc_transport_preference", "auto");
      localStorage.setItem("vc_transport_sticky", "true");
    });
    await page.reload();

    await joinMeeting(page, meetingId, "proto-user-9b");

    await openSettingsModal(page);
    await switchToNetworkTab(page);

    // Migrated value: WebTransport pill must be selected, NOT the (gone) Auto.
    await expect(page.locator('[data-testid="transport-radio-webtransport"]')).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expect(page.locator('[data-testid="transport-radio-auto"]')).toHaveCount(0);

    // Storage must be canonicalised from "auto" -> "webtransport" on load.
    const stored = await page.evaluate(() => localStorage.getItem("vc_transport_preference"));
    expect(stored).toBe("webtransport");

    // Cleanup
    await page.evaluate(() => {
      localStorage.removeItem("vc_transport_preference");
      localStorage.removeItem("vc_transport_sticky");
    });
  });

  // 10. Diagnostics panel shows transport preference dropdown
  test("diagnostics panel shows transport preference dropdown", async ({ page }) => {
    const meetingId = `e2e_proto_diag_${Date.now()}`;
    await joinMeeting(page, meetingId, "proto-user-10");

    await openDiagnosticsPanel(page);

    // The transport preference select should be visible
    const diagSelect = diagnosticsTransportSelect(page);
    await expect(diagSelect).toBeVisible();

    // Default value should be "webtransport" (was "auto" before simplification)
    await expect(diagSelect).toHaveValue("webtransport");

    // Exactly two options must be present (no Auto)
    const options = diagSelect.locator("option");
    await expect(options).toHaveCount(2);
    await expect(diagSelect.locator('option[value="auto"]')).toHaveCount(0);
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
    // Default is now webtransport — switch to websocket to trigger the dialog.
    await diagSelect.selectOption("websocket");
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
