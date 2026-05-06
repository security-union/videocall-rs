import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import {
  createMeeting,
  joinMeeting,
  endMeeting,
  deleteAllOwnedMeetings,
} from "../helpers/meeting-api";

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

// Selectors for the merged "Meetings" section on the home page. The previous
// design rendered two separate lists ("My Meetings" + "Previously Joined")
// backed by two API endpoints; both have been collapsed into a single section
// backed by `GET /api/v1/meetings/feed`, which returns the union of meetings
// the authenticated user owns or has been admitted into. Each row carries a
// server-supplied `is_owner` flag that gates the inline gold star icon, the
// edit button, the delete button, and the tooltip's "Owner" line.
const MEETINGS_SECTION = ".meetings-list-container";
const MEETINGS_SECTION_HEADER = ".meetings-list-container .meetings-list-toggle";
const MEETINGS_LIST_ROWS = ".meetings-list-container .meeting-item";
const OWNER_ICON = ".meeting-owner-icon";

/**
 * Wait until the merged meetings list reports it has finished loading and
 * has rendered exactly `expected` rows. The component sets `loading=true`
 * on mount and only renders the `<ul class="meetings-list">` once the
 * fetch resolves, so we can't safely assert against row count without
 * gating on the loading spinner first.
 */
async function waitForMeetingsRowCount(page: Page, expected: number): Promise<void> {
  // The loading state renders `.meetings-loading` inside the section.
  // We wait for that to disappear before counting list rows.
  await expect(page.locator(`${MEETINGS_SECTION} .meetings-loading`)).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(page.locator(MEETINGS_LIST_ROWS)).toHaveCount(expected, { timeout: 10_000 });
}

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

  test("Display Name info icon — click toggles the tooltip open and closed", async ({ page }) => {
    // Signal-driven click-to-toggle path (issue #460): clicking the info
    // trigger should park the tooltip open even after the pointer leaves,
    // and clicking the trigger again should close it. This complements
    // the CSS `:hover` reveal path tested above and exists primarily for
    // touch devices where there is no hover state.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(usernameInfoTriggerSelector);
    const tooltip = page.locator(usernameInfoTooltipSelector);

    await expect(tooltip).toBeHidden();

    // First click parks the tooltip open via the `--open` modifier class.
    await trigger.click();
    await expect(tooltip).toBeVisible({ timeout: 1000 });
    await expect(trigger).toHaveClass(/field-label__info--open/);

    // Second click toggles it back closed. After clicking the trigger we
    // also blur it to drop the `:focus-within` CSS reveal that would
    // otherwise keep the tooltip visible while the trigger is focused.
    await trigger.click();
    await trigger.blur();
    await expect(trigger).not.toHaveClass(/field-label__info--open/);
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Meeting ID info icon — Enter and Space keys toggle the tooltip", async ({ page }) => {
    // Keyboard activation parity with click-to-toggle (issue #460): with
    // the trigger focused, pressing Enter or Space should toggle the
    // tooltip the same way a click does. This is required for keyboard-
    // only users who can't fall back to a click event.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(meetingIdInfoTriggerSelector);
    const tooltip = page.locator(meetingIdInfoTooltipSelector);

    // Programmatic focus mirrors the Tab-onto-trigger flow without binding
    // the test to whatever Tab order surrounding elements introduce.
    await trigger.focus();
    // Focus alone reveals the tooltip via CSS `:focus-visible`/`:focus-within`,
    // independent of the signal-driven `--open` class — that's the existing
    // path. We still expect it visible here so the subsequent Enter press
    // is unambiguously the action that drives the `--open` toggle off.
    await expect(tooltip).toBeVisible({ timeout: 1000 });

    // Enter parks the signal open (toggling from None → MeetingId). The
    // tooltip stays visible (it was already visible via focus), but the
    // `--open` class should now be present.
    await page.keyboard.press("Enter");
    await expect(trigger).toHaveClass(/field-label__info--open/);
    await expect(tooltip).toBeVisible({ timeout: 1000 });

    // Space toggles it back off. With the signal cleared and only focus
    // keeping the tooltip visible via CSS, blur to drop the focus reveal
    // and confirm the tooltip closes.
    await page.keyboard.press("Space");
    await expect(trigger).not.toHaveClass(/field-label__info--open/);
    await trigger.blur();
    await expect(tooltip).toBeHidden({ timeout: 1000 });

    // Re-open with Space to prove both keys work in the open direction
    // too — symmetric with the Enter open above.
    await trigger.focus();
    await page.keyboard.press("Space");
    await expect(trigger).toHaveClass(/field-label__info--open/);

    // And close with Enter to prove Enter also closes (full symmetry).
    await page.keyboard.press("Enter");
    await expect(trigger).not.toHaveClass(/field-label__info--open/);
    await trigger.blur();
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Open tooltip dismisses on Escape key", async ({ page }) => {
    // Escape-to-dismiss is installed at the window level (issue #460): the
    // home page registers a `keydown` listener that clears the open-tooltip
    // signal when Escape is pressed, regardless of focus. This is the
    // standard escape-hatch for any modal-ish overlay.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(usernameInfoTriggerSelector);
    const tooltip = page.locator(usernameInfoTooltipSelector);

    // Open via click-to-toggle (verified above).
    await trigger.click();
    await expect(trigger).toHaveClass(/field-label__info--open/);
    await expect(tooltip).toBeVisible({ timeout: 1000 });

    // Escape clears the open-tooltip signal. Blur the trigger afterwards
    // so any residual `:focus-within` CSS reveal doesn't keep the tooltip
    // visible — the assertion we care about is that the `--open` class
    // was removed.
    await page.keyboard.press("Escape");
    await expect(trigger).not.toHaveClass(/field-label__info--open/);
    await trigger.blur();
    await expect(tooltip).toBeHidden({ timeout: 1000 });
  });

  test("Open tooltip dismisses on outside click", async ({ page }) => {
    // Outside-click dismissal is the touch-device equivalent of Escape
    // (issue #460): a click whose target is not inside any element marked
    // with `data-tooltip-trigger` should clear the open-tooltip signal.
    // This exists primarily so iOS Safari users can dismiss a tooltip that
    // got stuck via tap-focus on the trigger.
    await page.goto("/");
    await page.waitForTimeout(1500);

    const trigger = page.locator(meetingIdInfoTriggerSelector);
    const tooltip = page.locator(meetingIdInfoTooltipSelector);

    await trigger.click();
    await expect(trigger).toHaveClass(/field-label__info--open/);
    await expect(tooltip).toBeVisible({ timeout: 1000 });

    // Click the top-left corner of the page — far from the form region so
    // there's no risk of an en-route hover or click landing on a tooltip
    // trigger and re-toggling it. `force: true` keeps Playwright from
    // refusing the click on whatever element happens to be at (5, 5).
    await page.locator("body").click({ position: { x: 5, y: 5 }, force: true });
    await expect(trigger).not.toHaveClass(/field-label__info--open/);
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

/**
 * Merged "Meetings" section.
 *
 * The home page renders a single `MeetingsList` section
 * (`dioxus-ui/src/components/meetings_list.rs`) backed by
 * `GET /api/v1/meetings/feed`. That endpoint returns the union of meetings
 * the authenticated user owns or has been admitted into — owned and
 * non-owned alike — ordered server-side by `last_active_at DESC, id DESC`,
 * capped at 200 rows. Each row carries a server-supplied `is_owner` flag
 * which is the **only** authoritative ownership signal in the UI; it gates
 * the inline gold star icon (`.meeting-owner-icon`), the edit and delete
 * buttons, and the "Owner" tooltip line. The two-section layout it replaced
 * (separate "My Meetings" + "Previously Joined") is gone.
 *
 * Each test uses a unique-per-run user identity so multiple test workers
 * (and re-runs against a non-cleaned DB) don't pollute one another's feed.
 * We seed via the meeting-api REST endpoints — much faster and more
 * deterministic than driving the UI flow.
 */
test.describe("Meetings list (merged feed)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("section header reads 'Meetings' with a count badge", async ({ context, baseURL, page }) => {
    // Use a dedicated user to keep the assertion stable regardless of any
    // residual seed data left over from earlier tests.
    const email = `meetings-header-${Date.now()}@videocall.rs`;
    const name = "MeetingsHeaderUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);

    // Exactly one merged section should be present.
    await expect(page.locator(MEETINGS_SECTION)).toHaveCount(1);

    // The literal section header text must be present in the toggle button
    // (case-sensitive — this is the visible UI label).
    await expect(page.locator(MEETINGS_SECTION_HEADER)).toContainText("Meetings");

    // The previous design had a separate "Previously Joined" section above
    // the merged list. Guard against the regression that re-introduces a
    // second section (or the literal old header copy).
    await expect(page.locator(".joined-meetings-list-container")).toHaveCount(0);
    await expect(page.getByText("Previously Joined", { exact: false })).toHaveCount(0);
  });

  test("empty state shows 'No meetings yet'", async ({ context, baseURL, page }) => {
    // Use a fresh user identity that has never participated in any meeting.
    // The empty-state branch in MeetingsList renders a single
    // `.meetings-empty` div with the literal copy below.
    const email = `meetings-empty-${Date.now()}@videocall.rs`;
    const name = "MeetingsEmptyUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);

    // Wait for the section to settle (loading spinner gone).
    await expect(page.locator(`${MEETINGS_SECTION} .meetings-loading`)).toHaveCount(0, {
      timeout: 15_000,
    });

    // Empty-state copy — the merged component reuses the existing copy
    // ("No meetings yet"), since it's now the sole list on the page.
    const empty = page.locator(`${MEETINGS_SECTION} .meetings-empty`);
    await expect(empty).toBeVisible();
    await expect(empty).toHaveText("No meetings yet");

    // The list itself must NOT be rendered when empty.
    await expect(page.locator(MEETINGS_LIST_ROWS)).toHaveCount(0);

    // The count badge in the section header should read "(0)".
    await expect(page.locator(`${MEETINGS_SECTION_HEADER} .meeting-count`)).toHaveText("(0)");
  });

  test("state pill renders for active and ended meetings (idle TODO)", async ({
    context,
    baseURL,
    page,
  }) => {
    // The component renders `state-active` / `state-idle` / `state-ended`
    // pills with title-cased Rust-side copy. We can drive a meeting into
    // the `active` state via the join-as-host path, and into `ended` via
    // POST /api/v1/meetings/{id}/end. The `idle` state is NOT reachable
    // through the public API (owner-join always activates), so it's
    // marked TODO below.
    const email = `meetings-states-${Date.now()}@videocall.rs`;
    const name = "MeetingsStatesUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // Seed: one ACTIVE meeting (host-join activates it).
    const activeId = `e2e_meetings_active_${Date.now()}`;
    await createMeeting(email, name, { meetingId: activeId, waitingRoomEnabled: false });
    await joinMeeting(email, name, activeId, name);

    // Seed: one ENDED meeting (host-join, then explicitly end).
    const endedId = `e2e_meetings_ended_${Date.now()}`;
    await createMeeting(email, name, { meetingId: endedId, waitingRoomEnabled: false });
    await joinMeeting(email, name, endedId, name);
    await endMeeting(email, name, endedId);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    // Both rows should render their state pill with the right CSS class.
    await expect(page.locator(`${MEETINGS_SECTION} .meeting-state.state-active`)).toHaveCount(1);
    await expect(page.locator(`${MEETINGS_SECTION} .meeting-state.state-ended`)).toHaveCount(1);

    // The state label is title-cased in Rust before rendering ("Active",
    // "Ended"), not lower-cased + CSS uppercase. We assert the exact DOM
    // text so a regression that drops the title-case (or sneaks back to
    // the raw lowercase enum string) fails this test loudly.
    await expect(page.locator(`${MEETINGS_SECTION} .meeting-state.state-active`)).toHaveText(
      "Active",
    );
    await expect(page.locator(`${MEETINGS_SECTION} .meeting-state.state-ended`)).toHaveText(
      "Ended",
    );

    await deleteAllOwnedMeetings(email, name);

    // TODO: assert `state-idle` rendering. The owner-join API call always
    // transitions the meeting to `active`, and a non-owner cannot be
    // admitted before activation. Tracking as a follow-up; the rendering
    // code path is identical for all three states (single span with the
    // matching class) so the active/ended coverage gives reasonable
    // confidence.
  });

  test("Owner icon appears only on owned rows (and never on guest rows)", async ({
    context,
    baseURL,
    page,
  }) => {
    // Two meetings:
    //   (a) one this test user owns + has joined → expect `.meeting-owner-icon`
    //   (b) one a DIFFERENT user owns and our test user joined → expect NO icon
    // This is the canonical UI binding for the server-supplied `is_owner`
    // flag, scoped to a single browser session. The cross-browser
    // regression for the original bug (two identities seeing each other's
    // ownership state) lives in `meetings-ownership.spec.ts`.
    const userEmail = `meetings-owner-self-${Date.now()}@videocall.rs`;
    const userName = "MeetingsOwnerSelf";
    const otherEmail = `meetings-owner-other-${Date.now()}@videocall.rs`;
    const otherName = "MeetingsOwnerOther";
    await injectSessionCookie(context, { baseURL, email: userEmail, name: userName });

    // (a) Owned-and-joined: create as test user, then join.
    const ownedId = `e2e_meetings_owned_${Date.now()}`;
    await createMeeting(userEmail, userName, {
      meetingId: ownedId,
      waitingRoomEnabled: false,
    });
    await joinMeeting(userEmail, userName, ownedId, userName);

    // (b) Other-owned + joined: a different user creates the meeting (with
    // waiting_room disabled so our test user auto-admits on join), then
    // the test user joins to get an `admitted_at` row recorded.
    const guestId = `e2e_meetings_guest_${Date.now()}`;
    await createMeeting(otherEmail, otherName, {
      meetingId: guestId,
      waitingRoomEnabled: false,
    });
    // The owner must join first so the meeting is active and admits non-hosts.
    await joinMeeting(otherEmail, otherName, guestId, otherName);
    const joinResult = await joinMeeting(userEmail, userName, guestId, userName);
    // Sanity-check the seed actually produced an admitted participant. Any
    // other status (`waiting`, `waiting_for_meeting`) means this test would
    // be silently skipping the non-owned assertion.
    expect(joinResult.status).toBe("admitted");

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    const rows = page.locator(MEETINGS_LIST_ROWS);

    // Locate each row by its rendered meeting-id span. Filter `meeting-item`
    // li elements by the literal id text so we don't depend on row order.
    const ownedRow = rows.filter({ hasText: ownedId });
    const guestRow = rows.filter({ hasText: guestId });
    await expect(ownedRow).toHaveCount(1);
    await expect(guestRow).toHaveCount(1);

    // Owned row: gold-star icon present + accessible "Owner" label.
    const ownedIcon = ownedRow.locator(OWNER_ICON);
    await expect(ownedIcon).toHaveCount(1);
    await expect(ownedIcon).toBeVisible();
    await expect(ownedIcon).toHaveAttribute("aria-label", "Owner");

    // Owned row: edit + delete buttons gated behind `is_owner` are present.
    await expect(ownedRow.locator(".meeting-edit-btn")).toHaveCount(1);
    await expect(ownedRow.locator(".meeting-delete-btn")).toHaveCount(1);

    // Non-owned row: NO icon and NO owner-only buttons anywhere inside that <li>.
    await expect(guestRow.locator(OWNER_ICON)).toHaveCount(0);
    await expect(guestRow.locator(".meeting-edit-btn")).toHaveCount(0);
    await expect(guestRow.locator(".meeting-delete-btn")).toHaveCount(0);

    // Regression guard: the legacy "Owner" pill class must be gone from
    // the DOM. The replacement is the inline icon above.
    await expect(page.locator(".meeting-owner-badge")).toHaveCount(0);

    await deleteAllOwnedMeetings(userEmail, userName);
    await deleteAllOwnedMeetings(otherEmail, otherName);
  });

  test("rows are ordered by most-recent activity first", async ({ context, baseURL, page }) => {
    // Seed 3 owned meetings with staggered join calls. The merged feed is
    // ordered server-side by `last_active_at DESC, id DESC` where
    // `last_active_at = COALESCE(p.last_admit, m.started_at, m.created_at)`.
    // Sleeping ~1s between joins guarantees distinct second-resolution
    // timestamps even on systems with coarse clock granularity.
    const email = `meetings-order-${Date.now()}@videocall.rs`;
    const name = "MeetingsOrderUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const oldestId = `e2e_meetings_order_a_${Date.now()}`;
    const middleId = `e2e_meetings_order_b_${Date.now()}`;
    const newestId = `e2e_meetings_order_c_${Date.now()}`;

    for (const id of [oldestId, middleId, newestId]) {
      await createMeeting(email, name, { meetingId: id, waitingRoomEnabled: false });
      await joinMeeting(email, name, id, name);
      // Briefly pause so the next admit timestamp is strictly later.
      await new Promise((r) => setTimeout(r, 1100));
    }

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 3);

    // Read the `.meeting-id` text of each row in DOM order.
    const renderedIds = await page.locator(`${MEETINGS_LIST_ROWS} .meeting-id`).allTextContents();

    // Expected ordering: most-recent activity first → newest, middle, oldest.
    expect(renderedIds).toEqual([newestId, middleId, oldestId]);

    await deleteAllOwnedMeetings(email, name);
  });

  test("more than 5 rows render (5-row limit was removed)", async ({ context, baseURL, page }) => {
    // The previous "Previously Joined" list capped the UI at 5 rows. The
    // merged feed inherits the server's 200-row default cap instead, so
    // seeding 7 meetings should produce 7 rendered rows (and a "(7)"
    // count badge).
    const email = `meetings-limit-${Date.now()}@videocall.rs`;
    const name = "MeetingsLimitUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const tsBase = Date.now();
    const ids: string[] = [];
    for (let i = 0; i < 7; i += 1) {
      const id = `e2e_meetings_limit_${tsBase}_${i}`;
      ids.push(id);
      await createMeeting(email, name, { meetingId: id, waitingRoomEnabled: false });
      await joinMeeting(email, name, id, name);
    }

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 7);

    // The count badge should reflect the full row count, not a truncated
    // page size.
    await expect(page.locator(`${MEETINGS_SECTION_HEADER} .meeting-count`)).toHaveText("(7)");

    await deleteAllOwnedMeetings(email, name);
  });

  test("clicking a row mirrors the meeting id into the form input", async ({
    context,
    baseURL,
    page,
  }) => {
    // The MeetingItem onclick handler defaults to pushing the
    // `Route::Meeting { id }` route when no `on_select_meeting` callback
    // is provided. On the home page, however, the parent passes a callback
    // that mirrors the meeting id into the input field instead of
    // navigating directly. We assert the home-page behaviour: the
    // `#meeting-id` input picks up the row's id.
    //
    // Subsequent navigation requires the user to click "Start or Join
    // Meeting" — that path is already covered by other tests in this file.
    const email = `meetings-click-${Date.now()}@videocall.rs`;
    const name = "MeetingsClickUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_meetings_click_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 1);

    // Click the row's content (the `<div class="meeting-item-content">`
    // owns the onclick — the `<li>` itself does not).
    await page.locator(`${MEETINGS_LIST_ROWS} .meeting-item-content`).first().click();

    // The home page wires `on_select_meeting` to set the meeting-id input
    // value. Verify the input now holds the clicked meeting's id.
    await expect(page.locator("#meeting-id")).toHaveValue(meetingId, { timeout: 5_000 });

    await deleteAllOwnedMeetings(email, name);
  });

  test("inline meeting details (duration, time range, joined count) are NOT in the row", async ({
    context,
    baseURL,
    page,
  }) => {
    // Regression guard: the inline `.meeting-details` element used to render
    // duration / time range / participant count / waiting / password lock
    // as visible siblings of `.meeting-info`. Those moved to a body-level
    // hover tooltip portal. The row should now contain ONLY the meeting id,
    // state pill, and (when the user owns it) the gold-star icon — no
    // `.meeting-details`, `.meeting-duration`, `.meeting-time`,
    // `.meeting-participants`, `.meeting-waiting`, or `.meeting-password`.
    const email = `meetings-row-clean-${Date.now()}@videocall.rs`;
    const name = "MeetingsRowCleanUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // Seed one ACTIVE and one ENDED meeting so we exercise both branches
    // of the old inline-details code path in a single check.
    const activeId = `e2e_meetings_row_active_${Date.now()}`;
    await createMeeting(email, name, { meetingId: activeId, waitingRoomEnabled: false });
    await joinMeeting(email, name, activeId, name);

    const endedId = `e2e_meetings_row_ended_${Date.now()}`;
    await createMeeting(email, name, { meetingId: endedId, waitingRoomEnabled: false });
    await joinMeeting(email, name, endedId, name);
    await endMeeting(email, name, endedId);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    // None of the inline detail elements should appear inside the rows.
    for (const cls of [
      ".meeting-details",
      ".meeting-duration",
      ".meeting-time",
      ".meeting-time-separator",
      ".meeting-participants",
      ".meeting-waiting",
      ".meeting-password",
    ]) {
      await expect(page.locator(`${MEETINGS_LIST_ROWS} ${cls}`)).toHaveCount(0);
    }

    await deleteAllOwnedMeetings(email, name);
  });

  test("hovering an owned row reveals tooltip with 'Owner' line at the top", async ({
    context,
    baseURL,
    page,
  }) => {
    // The tooltip is a portal — it lives on document.body, NOT inside the
    // row's subtree. CSS classes:
    //   - `.meeting-info-tooltip-portal` — base portal class
    //   - `.is-visible` — added on hover, drives opacity + transform
    // The id `#meeting-info-tooltip-global` is also stable.
    //
    // For an owned meeting, `build_meeting_tooltip_html` injects an "Owner"
    // row at the very top (with the gold-tinted modifier class) before
    // any of the metadata rows.
    const email = `meetings-tooltip-owned-${Date.now()}@videocall.rs`;
    const name = "MeetingsTooltipOwnedUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_meetings_tooltip_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 1);

    const tooltip = page.locator("#meeting-info-tooltip-global");

    // Pre-hover: the tooltip element may not yet exist (it's lazily created
    // on first hover). Either zero-count OR not-visible is acceptable.
    expect(await tooltip.count()).toBeLessThanOrEqual(1);

    // Hover the row's clickable content area to trigger onmouseenter.
    await page.locator(`${MEETINGS_LIST_ROWS} .meeting-item-content`).first().hover();

    // Tooltip becomes visible (the `.is-visible` class is added).
    await expect(page.locator("#meeting-info-tooltip-global.is-visible")).toBeVisible({
      timeout: 2_000,
    });

    // The active-branch tooltip for an owned meeting should surface the
    // "Owner" line at the top followed by the standard metadata rows. We
    // check label text — the values vary at runtime so substring matching
    // keeps the test robust.
    await expect(tooltip).toContainText("Owner");
    await expect(tooltip).toContainText("Created on");
    await expect(tooltip).toContainText("Started on");
    await expect(tooltip).toContainText("Duration");
    await expect(tooltip).toContainText("Attendees");

    // The Owner row uses a dedicated modifier class for its gold tint.
    await expect(tooltip.locator(".meeting-info-tooltip-row--owner")).toHaveCount(1);

    await deleteAllOwnedMeetings(email, name);
  });
});

// The previous "My Meetings list" describe block lived here. With the home
// page now rendering a single merged section backed by `/api/v1/meetings/feed`,
// its tests have been folded into the "Meetings list (merged feed)" describe
// above. Tooltip "Created on" and "Owner" coverage is provided by the
// "hovering an owned row reveals tooltip with 'Owner' line at the top" test.

/**
 * Merged "Meetings" section: expand/collapse persistence.
 *
 * The single home-page list section persists its expand/collapse state to
 * `localStorage` under the key `home.meetings.expanded`. The frontend also
 * migrates from the legacy two-key scheme (`home.my-meetings.expanded` +
 * `home.previously-joined.expanded`) on first load: if the new key is
 * absent and the legacy "My Meetings" key is set, its value is honored.
 *
 * Stored as the literal string `"true"` or `"false"` (any other value or a
 * missing key falls back to expanded == `true`). When the section is
 * collapsed, the entire `.meetings-list-content` div is removed from the
 * DOM (not visually hidden) — that's the visibility signal the tests
 * assert on.
 *
 * Playwright already gives each `test()` block a fresh browser context
 * (no `storageState` is configured in `playwright.config.ts`), so
 * localStorage starts empty for every test and the auth helper (which
 * only sets a session cookie) is independent of localStorage state.
 */
test.describe("Meeting-list section expand/collapse persistence", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Selectors scoped to the merged section.
  const TOGGLE = ".meetings-list-toggle";
  const CONTENT = ".meetings-list-content";
  const CHEVRON_EXPANDED = ".chevron-icon.expanded";
  const LOADING_SPINNER = ".meetings-loading";
  const STORAGE_KEY = "home.meetings.expanded";
  const LEGACY_MY_MEETINGS_KEY = "home.my-meetings.expanded";

  /**
   * Wait for the merged section to settle (loading spinner gone) before
   * measuring expand/collapse state. The fetch-on-mount runs concurrently
   * with the toggle render, so without this gate the assertions can race
   * the spinner.
   *
   * Only checks the section when it's currently expanded — a collapsed
   * section removes its `.meetings-list-content` (and therefore its
   * spinner) from the DOM, so `toHaveCount(0)` would resolve immediately
   * for the wrong reason. We resolve "currently expanded" by looking for
   * the chevron's `.expanded` modifier class.
   */
  async function waitForSectionReady(page: Page): Promise<void> {
    const chevronExpanded = await page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`).count();
    if (chevronExpanded > 0) {
      await expect(page.locator(`${MEETINGS_SECTION} ${LOADING_SPINNER}`)).toHaveCount(0, {
        timeout: 15_000,
      });
    }
  }

  test("collapse persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-collapse-${Date.now()}@videocall.rs`;
    const name = "PersistCollapseUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    // Default state: section is expanded (chevron carries `.expanded`,
    // `.meetings-list-content` is in the DOM).
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    // Click the toggle to collapse.
    await page.locator(`${MEETINGS_SECTION} ${TOGGLE}`).click();

    // Content disappears from DOM, chevron loses `.expanded`.
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // Reload — the section must STILL be collapsed.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);
  });

  test("expand persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-expand-${Date.now()}@videocall.rs`;
    const name = "PersistExpandUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    // Collapse first.
    await page.locator(`${MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);

    // Re-expand. Content reappears, chevron regains `.expanded`.
    await page.locator(`${MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();

    // Reload — the section must STILL be expanded (the localStorage value
    // was rewritten to `"true"` on the second click).
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
  });

  test("fresh users see the section expanded by default", async ({ context, baseURL, page }) => {
    // Regression guard for the "missing key → expanded default" branch in
    // `load_bool` (see `dioxus-ui/src/local_storage.rs`). Explicitly clear
    // localStorage after navigation in case any state from a previous test
    // bled in via the browser context (Playwright already gives us a fresh
    // context per test, but we belt-and-braces this here so the assertion's
    // intent is unambiguous).
    const email = `persist-default-${Date.now()}@videocall.rs`;
    const name = "PersistDefaultUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    // The section renders its content + the expanded-chevron variant.
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();

    // And the storage key has not been written yet (the default branch
    // is read-only — `save_bool` only fires on toggle).
    const value = await page.evaluate((key) => localStorage.getItem(key), STORAGE_KEY);
    expect(value).toBeNull();
  });

  test("clicking the toggle writes 'home.meetings.expanded' as the literal string", async ({
    context,
    baseURL,
    page,
  }) => {
    // Direct assertion against the storage contract: the merged section
    // owns a single key, written as the literal strings `"true"` / `"false"`.
    // This catches a regression where the key name changes silently — the
    // persistence tests above would still pass (an unread key + a
    // never-written key both fall back to the default), so we need this
    // explicit check too.
    const email = `persist-storage-key-${Date.now()}@videocall.rs`;
    const name = "PersistStorageKeyUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionReady(page);

    // Collapse the section.
    await page.locator(`${MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);

    const collapsed = await page.evaluate((key) => localStorage.getItem(key), STORAGE_KEY);
    expect(collapsed).toBe("false");

    // Re-expand — key flips back to `"true"`.
    await page.locator(`${MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    const expanded = await page.evaluate((key) => localStorage.getItem(key), STORAGE_KEY);
    expect(expanded).toBe("true");

    // The legacy "My Meetings" key must NOT be written by the new code.
    // It's read once on first load for migration but never persisted.
    const legacy = await page.evaluate((key) => localStorage.getItem(key), LEGACY_MY_MEETINGS_KEY);
    expect(legacy).toBeNull();
  });

  test("legacy 'home.my-meetings.expanded=false' migrates to the new section state", async ({
    context,
    baseURL,
    page,
  }) => {
    // Pre-load the LEGACY key with `false` (collapsed) before the app's
    // scripts run. The merged component should honor that preference on
    // first load even though the new key is absent. Once the user toggles
    // the section the new key is written; the legacy key is intentionally
    // left untouched (read-only migration).
    const email = `persist-migrate-${Date.now()}@videocall.rs`;
    const name = "PersistMigrateUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.addInitScript((key) => {
      localStorage.setItem(key, "false");
    }, LEGACY_MY_MEETINGS_KEY);

    await page.goto("/");
    await page.waitForTimeout(1500);

    // Migration applied: the section starts collapsed (no `.meetings-list-content`,
    // no expanded chevron) on first load even though the new key was never set.
    await expect(page.locator(`${MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // The new key has not been written by the migration path itself —
    // `save_bool` only fires on user-initiated toggles, by design.
    const newKeyBefore = await page.evaluate((key) => localStorage.getItem(key), STORAGE_KEY);
    expect(newKeyBefore).toBeNull();
  });
});
