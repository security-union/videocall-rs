import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

// Selectors for the inline label-row error pattern. The error <span> lives
// INSIDE the <label> (right-aligned, sharing the row with the field name +
// info icon), not adjacent to the input. The span is always present in the
// DOM; CSS hides it via `:empty` when there's nothing to show.
const usernameErrorSelector = 'label[for="username"] .field-label__error';
const meetingIdErrorSelector = 'label[for="meeting-id"] .field-label__error';

// Selectors for the info-icon tooltip pattern. The trigger is a focusable
// <span role="button"> immediately after the field name; the tooltip is a
// <span role="tooltip"> with a stable id, hidden via CSS opacity/visibility
// until the trigger is hovered or focused.
const usernameInfoTriggerSelector = 'label[for="username"] .field-label__info';
const usernameInfoTooltipSelector = "#username-info-tip";
const meetingIdInfoTriggerSelector = 'label[for="meeting-id"] .field-label__info';
const meetingIdInfoTooltipSelector = "#meeting-id-info-tip";

test.describe("Meetings", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("home page loads with meeting form", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    await expect(page.locator("h1")).toContainText("videocall.rs");
    await expect(page.locator("#username")).toBeVisible();
    await expect(page.locator("#meeting-id")).toBeVisible();
    // With an empty meeting-id field, only the Generate button is rendered.
    await expect(page.getByText("Generate a New Meeting ID")).toBeVisible();
    // The Start/Join button is NOT in the DOM until the user types into #meeting-id.
    await expect(page.getByText("Start or Join Meeting")).toHaveCount(0);
    await page.waitForTimeout(1500);
  });

  test("browser tab title is exactly 'videocall.rs'", async ({ page }) => {
    // Regression guard: the title was briefly 'videocall.rs (Dioxus)' during
    // earlier UX work. The final state must be the bare brand name.
    await page.goto("/");
    await expect(page).toHaveTitle("videocall.rs");
  });

  test("display name input starts empty in a fresh session", async ({ page }) => {
    // In a fresh browser context (no localStorage), the controlled display
    // name input should start with an empty value.
    await page.goto("/");
    await page.waitForTimeout(1500);
    await expect(page.locator("#username")).toHaveValue("");
  });

  test("can join a meeting by filling the form", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);
    // Fill meeting-id first (has oninput handler that triggers re-render),
    // then username last so re-render doesn't clobber it.
    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially("e2e_test_room", { delay: 80 });
    await page.waitForTimeout(1000);
    // Once the meeting-id field has content, the button-swap kicks in:
    // Start/Join is rendered, Generate is removed from the DOM.
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();
    await expect(page.getByText("Generate a New Meeting ID")).toHaveCount(0);
    // The display name is a controlled input (value bound to signal).
    // Clear it first in case any pre-fill occurred, then type our value.
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(1000);
    // Submit via Enter on the form to avoid re-render race
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(/\/meeting\/e2e_test_room/, { timeout: 10_000 });
    await page.waitForTimeout(2000);
  });

  test("clicking Generate populates the meeting-id field; user clicks Start/Join to enter the meeting", async ({
    page,
  }) => {
    // The Generate button no longer navigates straight into a meeting.
    // It now mints a server-side meeting ID, writes it into the #meeting-id
    // input, and the button-swap exposes the Start/Join button. Navigation
    // happens only when the user submits the form.
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Display name is required because the server records the creator.
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("e2euser", { delay: 80 });
    await page.waitForTimeout(500);

    // Click Generate. Stay on home page, wait for the input to populate.
    await page.getByText("Generate a New Meeting ID").click();
    await expect(page.locator("#meeting-id")).not.toHaveValue("", { timeout: 10_000 });

    // Sanity-check the URL did NOT change to /meeting/<id>.
    await expect(page).toHaveURL(/\/$/);

    // Button-swap should have happened: Generate gone, Start/Join visible.
    await expect(page.getByText("Generate a New Meeting ID")).toHaveCount(0);
    await expect(page.getByText("Start or Join Meeting")).toBeVisible();

    // Click Start/Join to actually enter the meeting.
    await page.getByText("Start or Join Meeting").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });
    await page.waitForTimeout(2000);
  });

  test("display name is saved to localStorage on form submit", async ({ page }) => {
    // The display name should be saved to localStorage when "Start or Join
    // Meeting" is clicked (not on keystroke). Verify by joining, then
    // navigating back to the home page and checking the input is pre-filled.
    const meetingId = `e2e_persist_${Date.now()}`;
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("PersistUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Navigate back to home page
    await page.goto("/");
    await page.waitForTimeout(2000);

    // The display name input should be pre-filled from localStorage
    await expect(page.locator("#username")).toHaveValue("PersistUser", { timeout: 5_000 });
  });

  test("navigating directly to a meeting URL without a display name shows inline prompt", async ({
    page,
  }) => {
    // When no display name is stored, the meeting page shows an inline
    // display name prompt instead of redirecting to the home page.
    await page.goto("/meeting/no_username_test");
    await page.waitForTimeout(2000);
    // Should stay on the meeting page, NOT redirect to "/"
    await expect(page).toHaveURL(/\/meeting\/no_username_test/);
    // The inline prompt should be visible with input and join button
    await expect(page.getByText("Enter your display name")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("input.input-apple")).toBeVisible({ timeout: 5_000 });
    await expect(page.getByText("Join Meeting")).toBeVisible({ timeout: 5_000 });
  });

  test("display name is saved to localStorage on Generate, then on Start/Join navigation", async ({
    page,
  }) => {
    // Same persistence check, but using the new two-step Generate -> Start/Join
    // flow. The display name is saved to localStorage on Generate click (in
    // the validation success path before the async create_meeting runs); the
    // URL change only happens once the user clicks Start/Join.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("CreateUser", { delay: 80 });
    await page.waitForTimeout(500);

    // Step 1: Generate populates the field but does not navigate.
    await page.getByText("Generate a New Meeting ID").click();
    await expect(page.locator("#meeting-id")).not.toHaveValue("", { timeout: 10_000 });
    await expect(page).toHaveURL(/\/$/);

    // Step 2: Click Start/Join to enter the meeting.
    await page.getByText("Start or Join Meeting").click();
    await expect(page).toHaveURL(/\/meeting\/[a-f0-9]+/, { timeout: 10_000 });

    // Navigate back to home page and confirm display name was persisted.
    await page.goto("/");
    await page.waitForTimeout(2000);
    await expect(page.locator("#username")).toHaveValue("CreateUser", { timeout: 5_000 });
  });

  test("display name field shows inline validation error when invalid char is typed", async ({
    page,
  }) => {
    // Inline label-row error pattern: typing a disallowed character flips
    // the input to the --invalid state (red border + aria-invalid="true")
    // and surfaces a short error message right-aligned in the label row.
    // The error span is always in the DOM; it's hidden via :empty CSS when
    // there's no message, so we assert against its text content.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const errorLocator = page.locator(usernameErrorSelector);
    const usernameInput = page.locator("#username");

    // Empty state: no error text, input is in its default (valid) state.
    await expect(errorLocator).toHaveText("");
    await expect(usernameInput).not.toHaveClass(/input-apple--invalid/);

    await usernameInput.click();
    await usernameInput.fill("");
    await usernameInput.pressSequentially("alice@", { delay: 80 });
    await page.waitForTimeout(500);

    // Error must be the new short form, e.g. exactly "'@' not allowed".
    // We assert the regex form so future shape-preserving tweaks don't
    // break this test, but the current copy must match exactly here.
    await expect(errorLocator).toHaveText(/^'@' not allowed$/);
    // Visual error state: red-border class + ARIA hook for assistive tech.
    await expect(usernameInput).toHaveClass(/input-apple--invalid/);
    await expect(usernameInput).toHaveAttribute("aria-invalid", "true");

    // Remove the invalid char — error text clears, --invalid class drops.
    await usernameInput.fill("alice");
    await page.waitForTimeout(500);
    await expect(errorLocator).toHaveText("");
    await expect(usernameInput).not.toHaveClass(/input-apple--invalid/);
    await expect(usernameInput).toHaveAttribute("aria-invalid", "false");
  });

  test("meeting-id field shows inline validation error when invalid char is typed", async ({
    page,
  }) => {
    // Hyphens are NOT allowed in meeting IDs (the field permits only
    // alphanumerics + underscore). The inline error appears on invalid
    // keystrokes, the input gets the --invalid class + aria-invalid="true",
    // and the error clears once the field is valid again.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const errorLocator = page.locator(meetingIdErrorSelector);
    const meetingIdInput = page.locator("#meeting-id");

    // Empty state: no error text, input in default (valid) state.
    await expect(errorLocator).toHaveText("");
    await expect(meetingIdInput).not.toHaveClass(/input-apple--invalid/);

    await meetingIdInput.click();
    await meetingIdInput.pressSequentially("my-room", { delay: 80 });
    await page.waitForTimeout(500);

    // Short-form error: exactly "'-' not allowed".
    await expect(errorLocator).toHaveText(/^'-' not allowed$/);
    await expect(meetingIdInput).toHaveClass(/input-apple--invalid/);
    await expect(meetingIdInput).toHaveAttribute("aria-invalid", "true");

    // Fix the field — error text clears, --invalid class drops.
    await meetingIdInput.fill("myroom");
    await page.waitForTimeout(500);
    await expect(errorLocator).toHaveText("");
    await expect(meetingIdInput).not.toHaveClass(/input-apple--invalid/);
    await expect(meetingIdInput).toHaveAttribute("aria-invalid", "false");
  });

  test("home page does NOT show static validation hints in the empty state", async ({ page }) => {
    // The previous static hints under both inputs were removed; only inline
    // errors should ever appear, and only when the user types an invalid
    // char. The same allowed-character info is now exposed only via the
    // info-icon tooltips, which stay hidden until the icon is hovered/
    // focused — verified separately in the tooltip tests below.
    await page.goto("/");
    await page.waitForTimeout(1500);

    // No long-form static hints anywhere in the visible page.
    await expect(
      page.getByText("Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"),
    ).toHaveCount(0);
    await expect(page.getByText("Characters allowed: a-z, A-Z, 0-9, and _")).toHaveCount(0);

    // Tooltips are present in the DOM but hidden by CSS until the trigger
    // is hovered or focused.
    await expect(page.locator(usernameInfoTooltipSelector)).toBeHidden();
    await expect(page.locator(meetingIdInfoTooltipSelector)).toBeHidden();
  });

  test("Display Name info icon reveals tooltip on hover and hides on mouse-out", async ({
    page,
  }) => {
    // The info icon is a focusable <span role="button"> next to the
    // "Display Name" label. Hovering it should reveal the tooltip; moving
    // the mouse off should hide it again. Substring assertions only — the
    // exact wording may iterate but the allowed-char list is load-bearing.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(usernameInfoTriggerSelector);
    const tooltip = page.locator(usernameInfoTooltipSelector);

    await expect(tooltip).toBeHidden();

    await trigger.hover();
    await expect(tooltip).toBeVisible({ timeout: 1000 });
    await expect(tooltip).toContainText("Allowed: letters, numbers, spaces");

    // Move the pointer off the trigger to dismiss the tooltip.
    await page.mouse.move(0, 0);
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Display Name info icon reveals tooltip on keyboard focus and hides on blur", async ({
    page,
  }) => {
    // Keyboard accessibility: the info trigger has tabindex=0, so users
    // who can't hover (touch + screen readers, keyboard-only) must still
    // be able to read the tooltip. Tabbing onto the icon should reveal
    // it; tabbing away should dismiss it.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(usernameInfoTriggerSelector);
    const tooltip = page.locator(usernameInfoTooltipSelector);

    await expect(tooltip).toBeHidden();

    // Programmatically focus the trigger — robust to whatever Tab order
    // surrounding elements introduce. The behaviour we care about is
    // "tooltip becomes visible while the trigger has focus", and the
    // CSS selector is :focus-visible/:focus-within, which both fire on
    // a programmatic focus().
    await trigger.focus();
    await expect(tooltip).toBeVisible({ timeout: 1000 });
    await expect(tooltip).toContainText("Allowed: letters, numbers, spaces");

    // Blurring the trigger dismisses the tooltip.
    await trigger.blur();
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Meeting ID info icon reveals tooltip on hover with the right copy", async ({ page }) => {
    // The Meeting ID tooltip carries two load-bearing pieces: the allowed
    // character list AND the "Generate" affordance hint. Use substring
    // matches so the wording can be iterated without breaking this test.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(meetingIdInfoTriggerSelector);
    const tooltip = page.locator(meetingIdInfoTooltipSelector);

    await expect(tooltip).toBeHidden();

    await trigger.hover();
    await expect(tooltip).toBeVisible({ timeout: 1000 });
    await expect(tooltip).toContainText("Allowed: letters, numbers, and underscores");
    await expect(tooltip).toContainText("Generate a New Meeting ID");

    await page.mouse.move(0, 0);
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Meeting ID info icon reveals tooltip on keyboard focus and hides on blur", async ({
    page,
  }) => {
    // Keyboard accessibility parity with the Display Name tooltip: the
    // Meeting ID info trigger has tabindex=0, so keyboard-only and
    // touch-AT users must be able to read the tooltip without hovering.
    // Programmatic focus() drives :focus-visible/:focus-within, which is
    // the same CSS path used by Tab navigation — robust to whatever Tab
    // order surrounding elements introduce.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(meetingIdInfoTriggerSelector);
    const tooltip = page.locator(meetingIdInfoTooltipSelector);

    await expect(tooltip).toBeHidden();

    await trigger.focus();
    await expect(tooltip).toBeVisible({ timeout: 1000 });
    await expect(tooltip).toContainText("Allowed: letters, numbers, and underscores");
    await expect(tooltip).toContainText("Generate a New Meeting ID");

    // Blurring the trigger dismisses the tooltip.
    await trigger.blur();
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("form height is stable when Display Name validation error appears", async ({ page }) => {
    // The inline label-row error pattern is specifically designed so the
    // form's overall height does NOT change when an error appears — the
    // error rides into the existing label row instead of expanding a
    // sibling region. Buttons below must not jump. We measure the form's
    // bounding-rect height before vs. after typing an invalid character
    // and assert they're equal (allowing 1px for sub-pixel rendering).
    await page.goto("/");
    await page.waitForTimeout(1500);

    const usernameInput = page.locator("#username");
    const form = page.locator("form");

    // Empty state height.
    const heightBefore = await form.evaluate((el) => el.getBoundingClientRect().height);

    await usernameInput.click();
    await usernameInput.fill("");
    await usernameInput.pressSequentially("alice@", { delay: 80 });
    await page.waitForTimeout(500);

    // Error is now visible — confirm so the test fails meaningfully if the
    // error never rendered (otherwise the height check passes vacuously).
    await expect(page.locator(usernameErrorSelector)).toHaveText(/^'@' not allowed$/);

    const heightAfter = await form.evaluate((el) => el.getBoundingClientRect().height);

    expect(Math.abs(heightAfter - heightBefore)).toBeLessThanOrEqual(1);
  });

  test("form height is stable when Meeting ID validation error appears", async ({ page }) => {
    // Same no-layout-shift guarantee, this time for the Meeting ID field.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const meetingIdInput = page.locator("#meeting-id");
    const form = page.locator("form");

    const heightBefore = await form.evaluate((el) => el.getBoundingClientRect().height);

    await meetingIdInput.click();
    await meetingIdInput.pressSequentially("my-room", { delay: 80 });
    await page.waitForTimeout(500);

    await expect(page.locator(meetingIdErrorSelector)).toHaveText(/^'-' not allowed$/);

    const heightAfter = await form.evaluate((el) => el.getBoundingClientRect().height);

    expect(Math.abs(heightAfter - heightBefore)).toBeLessThanOrEqual(1);
  });
});
