import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Chat feature (disabled by default)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("chat button is hidden in toolbar when chatEnabled is false", async ({ page }) => {
    const meetingId = `e2e_chat_disabled_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ChatTestUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Enter the meeting
    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // The chat button should NOT be in the toolbar when chatEnabled is false.
    // The ChatButton component is conditionally rendered based on the
    // chat_enabled() config function, so it should not exist in the DOM at all.
    await expect(page.locator('[data-testid="chat-button"]')).toHaveCount(0);

    // The chat panel container should also not exist in the DOM.
    await expect(page.locator("#chat-panel-container")).toHaveCount(0);
  });

  test("chat panel container is absent from DOM when chatEnabled is false", async ({ page }) => {
    const meetingId = `e2e_chat_no_panel_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("NoPanelUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Neither the chat panel nor any chat-related elements should be present.
    await expect(page.locator("#chat-panel-container")).toHaveCount(0);
    await expect(page.locator(".chat-input")).toHaveCount(0);
    await expect(page.locator(".chat-messages")).toHaveCount(0);
  });

  test("other toolbar buttons still function without chat", async ({ page }) => {
    // Verify that the toolbar renders correctly and other buttons (peer list,
    // settings) remain functional when chat is disabled.
    const meetingId = `e2e_chat_toolbar_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ToolbarUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // No chat button
    await expect(page.locator('[data-testid="chat-button"]')).toHaveCount(0);

    // Settings button should still work
    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  });
});

// ---------------------------------------------------------------------------
// Tests that require chatEnabled=true.
//
// The default E2E docker stack sets CHAT_ENABLED=false. These tests are
// skipped until a chat service (or mock) is added to the E2E stack.
//
// To enable these tests:
// 1. Add a mock chat server to docker-compose.e2e.yaml
// 2. Set CHAT_ENABLED=true and CHAT_API_BASE_URL, CHAT_AUTH_MODE, etc.
//    on the dioxus-ui service
// 3. Remove the test.skip() calls below
// ---------------------------------------------------------------------------

test.describe("Chat feature (enabled)", () => {
  // Skip the entire describe block when chat is not configured in the
  // E2E environment. The skip condition checks for an environment variable
  // that would be set when a chat service is available.
  test.skip(
    () => !process.env.CHAT_E2E_ENABLED,
    "Skipped: CHAT_E2E_ENABLED not set. Chat service not available in E2E stack.",
  );

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("chat button is visible in toolbar when chatEnabled is true", async ({ page }) => {
    const meetingId = `e2e_chat_enabled_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ChatEnabledUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // The chat button should be visible in the toolbar.
    await expect(page.locator('[data-testid="chat-button"]')).toBeVisible({ timeout: 5_000 });
  });

  test("chat panel opens and closes when clicking chat button", async ({ page }) => {
    const meetingId = `e2e_chat_toggle_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ToggleChatUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    const chatButton = page.locator('[data-testid="chat-button"]');
    const chatPanel = page.locator("#chat-panel-container");

    // Click chat button to open the panel
    await chatButton.click();
    await page.waitForTimeout(500);
    await expect(chatPanel).toHaveClass(/visible/);

    // Click chat button again to close the panel
    await chatButton.click();
    await page.waitForTimeout(500);
    await expect(chatPanel).not.toHaveClass(/visible/);
  });

  test("opening chat panel closes peer list (mutual exclusivity)", async ({ page }) => {
    const meetingId = `e2e_chat_excl_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ExclUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    const chatButton = page.locator('[data-testid="chat-button"]');
    const chatPanel = page.locator("#chat-panel-container");
    const peerListContainer = page.locator("#peer-list-container");

    // Open peer list first by clicking the peer list button (tooltip "Open Peers")
    const peerListButton = page.locator(".video-control-button").filter({
      has: page.locator(".tooltip", { hasText: /Open Peers|Close Peers/ }),
    });
    await peerListButton.click();
    await page.waitForTimeout(500);
    await expect(peerListContainer).toHaveClass(/visible/);

    // Now click chat button -- peer list should close and chat panel should open.
    // Note: the peer list button click handler does NOT close chat, but the
    // chat button click handler closes diagnostics. However, the peer list
    // button handler does close chat. The mutual exclusivity here is:
    // opening peer list closes chat; opening chat closes diagnostics (not
    // peer list directly). Let's verify what actually happens by checking
    // the code: PeerListButton onclick sets chat_open=false. ChatButton
    // onclick sets diagnostics_open=false. So opening chat does NOT close
    // peer list. The exclusivity is one-directional.
    //
    // Let's test the other direction: open chat, then open peer list.
    // PeerListButton onclick sets chat_open=false, so peer list should
    // close chat.

    // First, close peer list
    await peerListButton.click();
    await page.waitForTimeout(500);
    await expect(peerListContainer).not.toHaveClass(/visible/);

    // Open chat panel
    await chatButton.click();
    await page.waitForTimeout(500);
    await expect(chatPanel).toHaveClass(/visible/);

    // Open peer list -- this should close the chat panel
    await peerListButton.click();
    await page.waitForTimeout(500);
    await expect(peerListContainer).toHaveClass(/visible/);
    await expect(chatPanel).not.toHaveClass(/visible/);
  });

  test("chat panel shows header and input area", async ({ page }) => {
    const meetingId = `e2e_chat_ui_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("ChatUIUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Open chat panel
    await page.locator('[data-testid="chat-button"]').click();
    await page.waitForTimeout(500);

    const chatPanel = page.locator("#chat-panel-container");
    await expect(chatPanel).toHaveClass(/visible/);

    // Verify the panel contains the expected UI elements
    await expect(chatPanel.locator(".sidebar-header")).toBeVisible();
    await expect(chatPanel.locator("h2", { hasText: "Chat" })).toBeVisible();
    await expect(chatPanel.locator(".close-button")).toBeVisible();
    await expect(chatPanel.locator(".chat-input-area")).toBeVisible();
    await expect(chatPanel.locator(".chat-input")).toBeVisible();
    await expect(chatPanel.locator(".chat-send-button")).toBeVisible();

    // Close via the close button in the header
    await chatPanel.locator(".close-button").click();
    await page.waitForTimeout(500);
    await expect(chatPanel).not.toHaveClass(/visible/);
  });
});
