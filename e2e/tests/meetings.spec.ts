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

// Selectors for the "Previously Joined" section (rendered above MeetingsList).
// Mirrors the helpers documented in the JoinedMeetingsList component — the
// section reuses `meetings-list-container` for shared styling and adds
// `joined-meetings-list-container` as the disambiguator. The "My Meetings"
// section can therefore be selected with the negation form below.
const JOINED_SECTION = ".joined-meetings-list-container";
const JOINED_SECTION_HEADER = ".joined-meetings-list-container .meetings-list-toggle";
const JOINED_LIST_ROWS = ".joined-meetings-list-container .meeting-item";
const JOINED_OWNER_BADGE = ".meeting-owner-badge";
const MY_MEETINGS_SECTION = ".meetings-list-container:not(.joined-meetings-list-container)";

/**
 * Wait until the joined-meetings list reports it has finished loading and
 * has rendered exactly `expected` rows. The component sets `loading=true`
 * on mount and only renders the `<ul class="meetings-list">` once the
 * fetch resolves, so we can't safely assert against row count without
 * gating on the loading spinner first.
 */
async function waitForJoinedRowCount(page: Page, expected: number): Promise<void> {
  // The loading state renders `.meetings-loading` inside the joined section.
  // We wait for that to disappear before counting list rows.
  await expect(page.locator(`${JOINED_SECTION} .meetings-loading`)).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(page.locator(JOINED_LIST_ROWS)).toHaveCount(expected, { timeout: 10_000 });
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
 * "Previously Joined" meetings section.
 *
 * The home page renders a new section (component:
 * `dioxus-ui/src/components/joined_meetings_list.rs`) above the existing
 * "My Meetings" list. It surfaces the last N meetings the authenticated
 * user has been admitted into — owned and non-owned alike — ordered by
 * most recent admission. The backend integration tests already cover query
 * correctness; these tests only verify the UI surfaces the data correctly.
 *
 * Each test uses a unique-per-run user identity so multiple test workers
 * (and re-runs against a non-cleaned DB) don't pollute one another's joined
 * list. We seed via the meeting-api REST endpoints — much faster and more
 * deterministic than driving the UI flow.
 *
 * Limitation: an "idle" meeting where the test user is `admitted` is not
 * naturally reachable through the public API. The owner-join path activates
 * the meeting before inserting the participant row, and a non-owner cannot
 * be admitted until the meeting is active. The `state-idle` pill rendering
 * is therefore covered only via state-specific seeding TODOs below; the
 * `active` and `ended` states are exercised end-to-end.
 */
test.describe("Previously Joined meetings section", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("section header is visible above My Meetings", async ({ context, baseURL, page }) => {
    // Use a dedicated user to keep the assertion stable regardless of any
    // residual seed data left over from earlier tests.
    const email = `joined-header-${Date.now()}@videocall.rs`;
    const name = "JoinedHeaderUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);

    const joinedSection = page.locator(JOINED_SECTION);
    const myMeetingsSection = page.locator(MY_MEETINGS_SECTION);

    // Both sections should be present in the DOM.
    await expect(joinedSection).toHaveCount(1);
    await expect(myMeetingsSection).toHaveCount(1);

    // The literal section header text must be present inside the joined
    // section (case-sensitive — this is the visible UI label).
    await expect(page.locator(JOINED_SECTION_HEADER)).toContainText("Previously Joined");

    // Positional sanity: the joined section sits ABOVE My Meetings on the
    // page. We compare bounding-rect tops via evaluate() so the assertion
    // is robust against zoom level / layout differences.
    const tops = await page.evaluate(
      ({ joinedSel, mySel }) => {
        const j = document.querySelector(joinedSel) as HTMLElement | null;
        const m = document.querySelector(mySel) as HTMLElement | null;
        return {
          joined: j?.getBoundingClientRect().top ?? null,
          my: m?.getBoundingClientRect().top ?? null,
        };
      },
      { joinedSel: JOINED_SECTION, mySel: MY_MEETINGS_SECTION },
    );
    expect(tops.joined).not.toBeNull();
    expect(tops.my).not.toBeNull();
    expect(tops.joined as number).toBeLessThan(tops.my as number);
  });

  test("empty state shows 'No previously joined meetings'", async ({ context, baseURL, page }) => {
    // Use a fresh user identity that has never participated in any meeting.
    // The empty-state branch in JoinedMeetingsList renders a single
    // `.meetings-empty` div with the literal copy below.
    const email = `joined-empty-${Date.now()}@videocall.rs`;
    const name = "JoinedEmptyUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);

    // Wait for the joined section to settle (loading spinner gone).
    await expect(page.locator(`${JOINED_SECTION} .meetings-loading`)).toHaveCount(0, {
      timeout: 15_000,
    });

    // Empty-state copy and shape — scope the locator to the joined section
    // so leftover empty-state copy in MyMeetings ("No meetings yet") doesn't
    // accidentally satisfy this assertion.
    const empty = page.locator(`${JOINED_SECTION} .meetings-empty`);
    await expect(empty).toBeVisible();
    await expect(empty).toHaveText("No previously joined meetings");

    // The list itself must NOT be rendered when empty.
    await expect(page.locator(JOINED_LIST_ROWS)).toHaveCount(0);

    // The count badge in the section header should read "(0)".
    await expect(page.locator(`${JOINED_SECTION_HEADER} .meeting-count`)).toHaveText("(0)");
  });

  test("state pill renders for an active meeting (idle/ended TODO)", async ({
    context,
    baseURL,
    page,
  }) => {
    // The component reuses the existing `state-active` / `state-idle` /
    // `state-ended` classes from MeetingsList. We can drive a meeting into
    // the `active` state via the join-as-host path, and into `ended` via
    // POST /api/v1/meetings/{id}/end. The `idle` state is NOT reachable
    // for a meeting the user has joined (owner-join always activates),
    // so it's marked TODO below.
    const email = `joined-states-${Date.now()}@videocall.rs`;
    const name = "JoinedStatesUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // Seed: one ACTIVE meeting (host-join activates it).
    const activeId = `e2e_joined_active_${Date.now()}`;
    await createMeeting(email, name, { meetingId: activeId, waitingRoomEnabled: false });
    await joinMeeting(email, name, activeId, name);

    // Seed: one ENDED meeting (host-join, then explicitly end).
    const endedId = `e2e_joined_ended_${Date.now()}`;
    await createMeeting(email, name, { meetingId: endedId, waitingRoomEnabled: false });
    await joinMeeting(email, name, endedId, name);
    await endMeeting(email, name, endedId);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForJoinedRowCount(page, 2);

    // Both rows should render their state pill with the right CSS class.
    // Scope these selectors to the joined section so MyMeetings rows can't
    // satisfy the assertion.
    await expect(page.locator(`${JOINED_SECTION} .meeting-state.state-active`)).toHaveCount(1);
    await expect(page.locator(`${JOINED_SECTION} .meeting-state.state-ended`)).toHaveCount(1);

    // TODO: assert `state-idle` rendering. The owner-join API call always
    // transitions the meeting to `active`, and `list_joined_by_user` only
    // returns rows the user has been admitted into. To exercise this
    // branch we'd need either a DB-level seeding helper or a test-only
    // endpoint that materialises an idle-meeting + admitted-participant
    // pair. Tracking as a follow-up; the rendering code path is identical
    // for all three states (single span with the matching class) so the
    // active/ended coverage gives reasonable confidence.
  });

  test("Owner badge appears only on owned rows", async ({ context, baseURL, page }) => {
    // Two meetings:
    //   (a) one this test user owns + has joined → expect `.meeting-owner-badge`
    //   (b) one a DIFFERENT user owns and our test user joined → expect NO badge
    const userEmail = `joined-owner-self-${Date.now()}@videocall.rs`;
    const userName = "JoinedOwnerSelf";
    const otherEmail = `joined-owner-other-${Date.now()}@videocall.rs`;
    const otherName = "JoinedOwnerOther";
    await injectSessionCookie(context, { baseURL, email: userEmail, name: userName });

    // (a) Owned-and-joined: create as test user, then join.
    const ownedId = `e2e_joined_owned_${Date.now()}`;
    await createMeeting(userEmail, userName, {
      meetingId: ownedId,
      waitingRoomEnabled: false,
    });
    await joinMeeting(userEmail, userName, ownedId, userName);

    // (b) Other-owned + joined: a different user creates the meeting (with
    // waiting_room disabled so our test user auto-admits on join), then
    // the test user joins to get an `admitted_at` row recorded.
    const guestId = `e2e_joined_guest_${Date.now()}`;
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
    await waitForJoinedRowCount(page, 2);

    const rows = page.locator(JOINED_LIST_ROWS);

    // Locate each row by its rendered meeting-id span. Filter `meeting-item`
    // li elements by the literal id text so we don't depend on row order.
    const ownedRow = rows.filter({ hasText: ownedId });
    const guestRow = rows.filter({ hasText: guestId });
    await expect(ownedRow).toHaveCount(1);
    await expect(guestRow).toHaveCount(1);

    // Owned row: badge present + literal "Owner" copy.
    const ownedBadge = ownedRow.locator(JOINED_OWNER_BADGE);
    await expect(ownedBadge).toHaveCount(1);
    await expect(ownedBadge).toBeVisible();
    await expect(ownedBadge).toContainText("Owner");

    // Non-owned row: NO badge anywhere inside that <li>.
    await expect(guestRow.locator(JOINED_OWNER_BADGE)).toHaveCount(0);
  });

  test("rows are ordered by most-recent admission first", async ({ context, baseURL, page }) => {
    // Seed 3 owned meetings with staggered join calls. `last_joined_at` is
    // computed server-side as `COALESCE(admitted_at, joined_at)` — and for a
    // fresh host the admit timestamp is set in the same INSERT as the row.
    // Sleeping ~1s between joins guarantees distinct second-resolution
    // timestamps even on systems with coarse clock granularity.
    const email = `joined-order-${Date.now()}@videocall.rs`;
    const name = "JoinedOrderUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const oldestId = `e2e_joined_order_a_${Date.now()}`;
    const middleId = `e2e_joined_order_b_${Date.now()}`;
    const newestId = `e2e_joined_order_c_${Date.now()}`;

    for (const id of [oldestId, middleId, newestId]) {
      await createMeeting(email, name, { meetingId: id, waitingRoomEnabled: false });
      await joinMeeting(email, name, id, name);
      // Briefly pause so the next admit timestamp is strictly later.
      await new Promise((r) => setTimeout(r, 1100));
    }

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForJoinedRowCount(page, 3);

    // Read the `.meeting-id` text of each row in DOM order.
    const renderedIds = await page.locator(`${JOINED_LIST_ROWS} .meeting-id`).allTextContents();

    // Expected ordering: most-recent admission first → newest, middle, oldest.
    expect(renderedIds).toEqual([newestId, middleId, oldestId]);
  });

  test("at most 5 rows render even when more meetings have been joined", async ({
    context,
    baseURL,
    page,
  }) => {
    // The component requests `limit=5` from the backend, and the backend's
    // `total` is computed as `meetings.len()` (i.e. it reflects the page
    // size, not the unbounded count). Seed 7 joins and assert exactly 5
    // rows render. We also assert the count badge — written against the
    // ACTUAL backend semantic (`total = 5`), not the ideal "5 of 7".
    const email = `joined-limit-${Date.now()}@videocall.rs`;
    const name = "JoinedLimitUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // 7 meetings, host-joined sequentially. No need to space them out for
    // this test — we only care about row count, not order.
    const tsBase = Date.now();
    const ids: string[] = [];
    for (let i = 0; i < 7; i += 1) {
      const id = `e2e_joined_limit_${tsBase}_${i}`;
      ids.push(id);
      await createMeeting(email, name, { meetingId: id, waitingRoomEnabled: false });
      await joinMeeting(email, name, id, name);
    }

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForJoinedRowCount(page, 5);

    // Backend returns `total = meetings.len()` = 5, so the count badge in
    // the section header should read "(5)" even though the user has 7
    // joined meetings. This is a UX gap worth flagging — see test report.
    await expect(page.locator(`${JOINED_SECTION_HEADER} .meeting-count`)).toHaveText("(5)");
  });

  test("clicking a joined row navigates to the meeting page", async ({
    context,
    baseURL,
    page,
  }) => {
    // The JoinedMeetingItem onclick handler defaults to pushing the
    // `Route::Meeting { id }` route when no `on_select_meeting` callback
    // is provided. On the home page, however, the parent passes a callback
    // that mirrors the meeting id into the input field instead of
    // navigating directly. We assert the home-page behaviour: the
    // `#meeting-id` input picks up the row's id.
    //
    // Subsequent navigation requires the user to click "Start or Join
    // Meeting" — that path is already covered by other tests in this file.
    const email = `joined-click-${Date.now()}@videocall.rs`;
    const name = "JoinedClickUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_joined_click_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForJoinedRowCount(page, 1);

    // Click the row's content (the `<div class="meeting-item-content">`
    // owns the onclick — the `<li>` itself does not).
    await page.locator(`${JOINED_LIST_ROWS} .meeting-item-content`).first().click();

    // The home page wires `on_select_meeting` to set the meeting-id input
    // value. Verify the input now holds the clicked meeting's id.
    await expect(page.locator("#meeting-id")).toHaveValue(meetingId, { timeout: 5_000 });
  });

  test("joined rows have no edit or delete buttons", async ({ context, baseURL, page }) => {
    // The joined-meetings list intentionally omits the per-row edit and
    // delete affordances rendered by MeetingsList. Users manage owned
    // meetings from the My Meetings section. Assert neither button class
    // appears inside the joined section, even when one of the rows is
    // owned by the current user (which would otherwise be eligible for
    // edit/delete in MyMeetings).
    const email = `joined-no-ctrls-${Date.now()}@videocall.rs`;
    const name = "JoinedNoCtrlsUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_joined_no_ctrls_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForJoinedRowCount(page, 1);

    // Scoped locators — neither edit nor delete buttons should exist
    // inside the joined section. They DO exist in MyMeetings (this same
    // user owns the meeting), so a non-scoped assertion would fail.
    await expect(page.locator(`${JOINED_SECTION} .meeting-edit-btn`)).toHaveCount(0);
    await expect(page.locator(`${JOINED_SECTION} .meeting-delete-btn`)).toHaveCount(0);

    // Sanity-check: the same meeting in MyMeetings DOES expose those
    // controls, so the negative assertion above isn't vacuous (the
    // selectors exist and work — they're just absent from the joined
    // section by design).
    await expect(page.locator(`${MY_MEETINGS_SECTION} .meeting-edit-btn`)).toHaveCount(1);
    await expect(page.locator(`${MY_MEETINGS_SECTION} .meeting-delete-btn`)).toHaveCount(1);

    // Best-effort cleanup so this seed doesn't bleed into future runs.
    await deleteAllOwnedMeetings(email, name);
  });
});

/**
 * Home-page meeting-list section expand/collapse persistence.
 *
 * Both home-page list sections persist their expand/collapse state to
 * `localStorage` so a user's preference survives a page reload:
 *
 *   - "Previously Joined" → key `home.previously-joined.expanded`
 *   - "My Meetings"       → key `home.my-meetings.expanded`
 *
 * Stored as the literal string `"true"` or `"false"` (any other value or a
 * missing key falls back to expanded == `true`). When the section is
 * collapsed, the entire `.meetings-list-content` div is removed from the DOM
 * (not visually hidden) — that's the visibility signal the tests assert on.
 *
 * Playwright already gives each `test()` block a fresh browser context (no
 * `storageState` is configured in `playwright.config.ts`), so localStorage
 * starts empty for every test and the auth helper (which only sets a session
 * cookie) is independent of localStorage state.
 */
test.describe("Meeting-list section expand/collapse persistence", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // Selectors scoped to a section. Both sections expose the same toggle
  // button class and the same content wrapper class — we disambiguate via
  // the parent container.
  const TOGGLE = ".meetings-list-toggle";
  const CONTENT = ".meetings-list-content";
  const CHEVRON_EXPANDED = ".chevron-icon.expanded";
  const LOADING_SPINNER = ".meetings-loading";

  /**
   * Wait for both home-page list sections to settle (loading spinners gone)
   * before the test measures expand/collapse state. Each section's
   * fetch-on-mount runs concurrently with the toggle render, so without
   * this gate the assertions can race the spinner.
   *
   * Only checks sections that are currently expanded — a collapsed section
   * removes its `.meetings-list-content` (and therefore its spinner) from
   * the DOM, so `toHaveCount(0)` would resolve immediately for the wrong
   * reason. We resolve "currently expanded" by looking for the chevron's
   * `.expanded` modifier class on each section.
   */
  async function waitForSectionsReady(page: Page): Promise<void> {
    for (const section of [JOINED_SECTION, MY_MEETINGS_SECTION]) {
      const chevronExpanded = await page.locator(`${section} ${CHEVRON_EXPANDED}`).count();
      if (chevronExpanded > 0) {
        await expect(page.locator(`${section} ${LOADING_SPINNER}`)).toHaveCount(0, {
          timeout: 15_000,
        });
      }
    }
  }

  test("Previously Joined: collapse persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-joined-collapse-${Date.now()}@videocall.rs`;
    const name = "PersistJoinedCollapseUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Default state: section is expanded (chevron carries `.expanded`,
    // `.meetings-list-content` is in the DOM).
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();

    // Click the toggle to collapse.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();

    // Content disappears from DOM, chevron loses `.expanded`.
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // Reload — the section must STILL be collapsed.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // My Meetings should be unaffected — still expanded by default.
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
  });

  test("Previously Joined: expand persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-joined-expand-${Date.now()}@videocall.rs`;
    const name = "PersistJoinedExpandUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Collapse first.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);

    // Re-expand. Content reappears, chevron regains `.expanded`.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();

    // Reload — the section must STILL be expanded (the localStorage value
    // was rewritten to `"true"` on the second click).
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
  });

  test("My Meetings: collapse persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-my-collapse-${Date.now()}@videocall.rs`;
    const name = "PersistMyCollapseUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Default state: My Meetings is expanded.
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    // Collapse.
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // Reload — still collapsed.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toHaveCount(0);

    // Previously Joined should be unaffected.
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
  });

  test("My Meetings: expand persists across reload", async ({ context, baseURL, page }) => {
    const email = `persist-my-expand-${Date.now()}@videocall.rs`;
    const name = "PersistMyExpandUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Collapse, then re-expand.
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    // Reload — still expanded.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
  });

  test("sections are independent: collapsing one does not collapse the other", async ({
    context,
    baseURL,
    page,
  }) => {
    // Regression guard: each section must own its own localStorage key.
    // Collapse Previously Joined, reload, then collapse My Meetings, reload,
    // and assert the two states evolve independently across both reloads.
    const email = `persist-independence-${Date.now()}@videocall.rs`;
    const name = "PersistIndependenceUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Step 1: collapse Previously Joined only.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    // Reload — Previously Joined collapsed, My Meetings still expanded.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    // Step 2: collapse My Meetings (Previously Joined remains collapsed).
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);

    // Reload — both collapsed now.
    await page.reload();
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);
  });

  test("fresh users see both sections expanded by default", async ({ context, baseURL, page }) => {
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
    await waitForSectionsReady(page);

    // Both sections render their content + the expanded-chevron variant.
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${JOINED_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CHEVRON_EXPANDED}`)).toBeVisible();

    // And neither localStorage key has been written yet (the default branch
    // is read-only — `save_bool` only fires on toggle).
    const keys = await page.evaluate(() => ({
      joined: localStorage.getItem("home.previously-joined.expanded"),
      my: localStorage.getItem("home.my-meetings.expanded"),
    }));
    expect(keys.joined).toBeNull();
    expect(keys.my).toBeNull();
  });

  test("clicking each toggle once writes the correct localStorage key", async ({
    context,
    baseURL,
    page,
  }) => {
    // Direct assertion against the storage contract documented in
    // `dioxus-ui/src/local_storage.rs`: keys are written as the literal
    // strings `"true"` / `"false"`. This catches a regression where the
    // key name changes silently — the persistence tests above would still
    // pass (an unread key + a never-written key both fall back to the
    // default), so we need this explicit check too.
    const email = `persist-storage-keys-${Date.now()}@videocall.rs`;
    const name = "PersistStorageKeysUser";
    await injectSessionCookie(context, { baseURL, email, name });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForSectionsReady(page);

    // Collapse both sections.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toHaveCount(0);
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toHaveCount(0);

    const collapsed = await page.evaluate(() => ({
      joined: localStorage.getItem("home.previously-joined.expanded"),
      my: localStorage.getItem("home.my-meetings.expanded"),
    }));
    expect(collapsed.joined).toBe("false");
    expect(collapsed.my).toBe("false");

    // Re-expand both sections — keys flip back to `"true"`.
    await page.locator(`${JOINED_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${JOINED_SECTION} ${CONTENT}`)).toBeVisible();
    await page.locator(`${MY_MEETINGS_SECTION} ${TOGGLE}`).click();
    await expect(page.locator(`${MY_MEETINGS_SECTION} ${CONTENT}`)).toBeVisible();

    const expanded = await page.evaluate(() => ({
      joined: localStorage.getItem("home.previously-joined.expanded"),
      my: localStorage.getItem("home.my-meetings.expanded"),
    }));
    expect(expanded.joined).toBe("true");
    expect(expanded.my).toBe("true");
  });
});
