import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import {
  createMeeting,
  joinMeeting,
  endMeeting,
  deleteAllOwnedMeetings,
} from "../helpers/meeting-api";

/**
 * Home-page meetings-list FILTER + SORT toolbar (issue #1056).
 *
 * The merged "Meetings" section (`dioxus-ui/src/components/meetings_list.rs`)
 * now renders a toolbar above the list with a filter popover and a sort
 * popover. The filter/sort SEMANTICS are pure functions in
 * `dioxus-ui/src/components/meetings_filter.rs` and are exhaustively covered by
 * host-target `#[test]`s there (each group, each window boundary, each sort key
 * in both directions). This spec covers the END-TO-END UI binding only: the
 * controls render, mutate the rendered row set, drive the count badge, persist
 * to localStorage, and the popovers honour the documented a11y/interaction
 * contract (open on trigger, close on Escape / outside-click, focus returns to
 * the trigger).
 *
 * Selectors / ids are taken verbatim from the component:
 *   - `#meetings-filter-trigger`  — filter icon button
 *   - `.meetings-filter-badge`    — "N active" count badge inside the trigger
 *   - `#meetings-sort-trigger`    — sort button (carries the current key label)
 *   - `.meetings-sort-dir-btn`    — asc/desc direction toggle
 *   - `.meetings-popover`         — either floating panel (filter or sort)
 *   - `.meetings-filter-popover` / `.meetings-sort-popover` — the two panels
 *   - `.meetings-popover-backdrop`— transparent outside-click catcher
 *   - `.meetings-sort-option`     — a sort key button inside the sort popover
 *   - `.meetings-empty-filtered`  — "no meetings match your filters" empty state
 *   - `.meetings-clear-filters-btn` — Clear-filters button in that empty state
 *
 * localStorage keys:
 *   - `home.meetings.filter` — JSON-serialised FilterState
 *   - `home.meetings.sort`   — JSON-serialised SortState
 *
 * Seeding strategy
 * ----------------
 * We reuse the REST seeding helpers (`createMeeting` / `joinMeeting` /
 * `endMeeting`) exactly as the existing `meetings.spec.ts` does — no new
 * mechanism. Each test uses a unique per-run user identity so parallel workers
 * and re-runs against a non-cleaned DB don't pollute one another's feed.
 *
 * Backend mapping the assertions rely on (see `meeting-api/src/db/meetings.rs`):
 *   - `is_owner`               — true when the session user created the meeting.
 *   - `state`                  — "active" after the owner joins; "ended" after
 *                                `endMeeting`. ("idle" is not reachable via the
 *                                public API — owner-join always activates.)
 *   - `user_last_attended_at`  — MAX(admitted_at) over the user's own
 *                                participant rows; ≈ now after `joinMeeting`,
 *                                and OMITTED (None) when the user owns but never
 *                                joined the meeting.
 *
 * Attendance-window coverage caveat: there is no REST primitive to BACKDATE an
 * admission, and these specs deliberately never touch the DB directly. So the
 * window filter is exercised with the two states we CAN seed deterministically
 * — recently-attended (kept by "Last 7 days") and never-attended (excluded by
 * any bounded window). The "attended long ago / boundary" cases are covered by
 * the pure unit tests in `meetings_filter.rs`
 * (`attended_window_boundary_just_inside_and_just_outside`).
 */

const MEETINGS_SECTION = ".meetings-list-container";
const MEETINGS_LIST_ROWS = ".meetings-list-container .meeting-item";
const MEETINGS_ROW_IDS = `${MEETINGS_LIST_ROWS} .meeting-id`;

const FILTER_TRIGGER = "#meetings-filter-trigger";
const FILTER_BADGE = ".meetings-filter-badge";
const FILTER_POPOVER = ".meetings-filter-popover";
const SORT_TRIGGER = "#meetings-sort-trigger";
const SORT_POPOVER = ".meetings-sort-popover";
const SORT_DIR_BTN = ".meetings-sort-dir-btn";
const SORT_OPTION = ".meetings-sort-option";
const POPOVER_BACKDROP = ".meetings-popover-backdrop";
const FILTERED_EMPTY = ".meetings-empty-filtered";
const CLEAR_FILTERS_BTN = ".meetings-clear-filters-btn";

const FILTER_STORAGE_KEY = "home.meetings.filter";
const SORT_STORAGE_KEY = "home.meetings.sort";

/**
 * Wait until the merged list reports it has finished loading and has rendered
 * exactly `expected` rows. The component sets `loading=true` on mount and only
 * renders the `<ul class="meetings-list">` once the fetch resolves, so we gate
 * on the loading spinner before counting rows.
 */
async function waitForMeetingsRowCount(page: Page, expected: number): Promise<void> {
  await expect(page.locator(`${MEETINGS_SECTION} .meetings-loading`)).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(page.locator(MEETINGS_LIST_ROWS)).toHaveCount(expected, { timeout: 10_000 });
}

/** Read the `.meeting-id` text of every rendered row, in DOM order. */
async function renderedRowIds(page: Page): Promise<string[]> {
  return page.locator(MEETINGS_ROW_IDS).allTextContents();
}

/** Open the filter popover by clicking its trigger; assert it is visible. */
async function openFilterPopover(page: Page): Promise<void> {
  await page.locator(FILTER_TRIGGER).click();
  await expect(page.locator(FILTER_POPOVER)).toBeVisible();
}

/** Open the sort popover by clicking its trigger; assert it is visible. */
async function openSortPopover(page: Page): Promise<void> {
  await page.locator(SORT_TRIGGER).click();
  await expect(page.locator(SORT_POPOVER)).toBeVisible();
}

/**
 * Toggle a labelled checkbox inside the currently-open filter popover. The
 * checkboxes are wrapped in `<label>`s whose text is the visible option name
 * ("I own", "I don't own", "Active", "Ended"), so we target the input via the
 * label text using Playwright's accessible-name matching.
 */
async function checkFilterOption(page: Page, label: string): Promise<void> {
  await page.locator(FILTER_POPOVER).getByText(label, { exact: true }).click();
}

/** Select an attendance-window radio by its visible label inside the popover. */
async function selectAttendedWindow(page: Page, label: string): Promise<void> {
  await page.locator(FILTER_POPOVER).getByText(label, { exact: true }).click();
}

test.describe("Meetings list filter + sort toolbar (issue #1056)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("toolbar renders with default filter/sort affordances when the feed has rows", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-toolbar-${Date.now()}@videocall.rs`;
    const name = "MfsToolbarUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_mfs_toolbar_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 1);

    // Both triggers are present and the sort button advertises the default key.
    await expect(page.locator(FILTER_TRIGGER)).toBeVisible();
    await expect(page.locator(SORT_TRIGGER)).toBeVisible();
    await expect(page.locator(SORT_TRIGGER)).toContainText("Last active");

    // No constraints active by default → no count badge, plain aria-label.
    await expect(page.locator(FILTER_BADGE)).toHaveCount(0);
    await expect(page.locator(FILTER_TRIGGER)).toHaveAttribute("aria-label", "Filter meetings");

    await deleteAllOwnedMeetings(email, name);
  });

  test("filter by ownership: 'I own' keeps owned rows and shows the count badge", async ({
    context,
    baseURL,
    page,
  }) => {
    const ownerEmail = `mfs-own-self-${Date.now()}@videocall.rs`;
    const ownerName = "MfsOwnSelf";
    const otherEmail = `mfs-own-other-${Date.now()}@videocall.rs`;
    const otherName = "MfsOwnOther";
    await injectSessionCookie(context, { baseURL, email: ownerEmail, name: ownerName });

    // (a) Owned-and-joined.
    const ownedId = `e2e_mfs_owned_${Date.now()}`;
    await createMeeting(ownerEmail, ownerName, { meetingId: ownedId, waitingRoomEnabled: false });
    await joinMeeting(ownerEmail, ownerName, ownedId, ownerName);

    // (b) Other-owned + our user joined (auto-admit since waiting room is off).
    const guestId = `e2e_mfs_guest_${Date.now()}`;
    await createMeeting(otherEmail, otherName, { meetingId: guestId, waitingRoomEnabled: false });
    await joinMeeting(otherEmail, otherName, guestId, otherName);
    const joinResult = await joinMeeting(ownerEmail, ownerName, guestId, ownerName);
    expect(joinResult.status).toBe("admitted");

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    // Apply "I own".
    await openFilterPopover(page);
    await checkFilterOption(page, "I own");

    // Only the owned row survives.
    await waitForMeetingsRowCount(page, 1);
    expect(await renderedRowIds(page)).toEqual([ownedId]);

    // The count badge reads "1" and the aria-label reflects one active filter.
    await expect(page.locator(FILTER_BADGE)).toHaveText("1");
    await expect(page.locator(FILTER_TRIGGER)).toHaveAttribute(
      "aria-label",
      "Filter meetings (1 active)",
    );

    await deleteAllOwnedMeetings(ownerEmail, ownerName);
    await deleteAllOwnedMeetings(otherEmail, otherName);
  });

  test("filter by status: 'Active' keeps active rows and excludes ended", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-status-${Date.now()}@videocall.rs`;
    const name = "MfsStatusUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // One ACTIVE meeting (host-join activates it).
    const activeId = `e2e_mfs_active_${Date.now()}`;
    await createMeeting(email, name, { meetingId: activeId, waitingRoomEnabled: false });
    await joinMeeting(email, name, activeId, name);

    // One ENDED meeting (host-join, then end).
    const endedId = `e2e_mfs_ended_${Date.now()}`;
    await createMeeting(email, name, { meetingId: endedId, waitingRoomEnabled: false });
    await joinMeeting(email, name, endedId, name);
    await endMeeting(email, name, endedId);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    await openFilterPopover(page);
    await checkFilterOption(page, "Active");

    // Only the active row remains; the ended row is gone.
    await waitForMeetingsRowCount(page, 1);
    expect(await renderedRowIds(page)).toEqual([activeId]);
    await expect(page.locator(FILTER_BADGE)).toHaveText("1");

    await deleteAllOwnedMeetings(email, name);
  });

  test("attendance window: 'Last 7 days' keeps a recently-attended row and excludes a never-attended one", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-attended-${Date.now()}@videocall.rs`;
    const name = "MfsAttendedUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // (a) Recently attended: create + join → user_last_attended_at ≈ now.
    const recentId = `e2e_mfs_recent_${Date.now()}`;
    await createMeeting(email, name, { meetingId: recentId, waitingRoomEnabled: false });
    await joinMeeting(email, name, recentId, name);

    // (b) Never attended: owned but never joined → user_last_attended_at == None.
    // An owned meeting appears in the feed (creator_id match) even with no
    // admitted participant row, so it renders as a row but is excluded by any
    // bounded attendance window.
    const neverId = `e2e_mfs_never_${Date.now()}`;
    await createMeeting(email, name, { meetingId: neverId, waitingRoomEnabled: false });

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    await openFilterPopover(page);
    await selectAttendedWindow(page, "Last 7 days");

    // Only the recently-attended row survives; the never-attended row is gone.
    await waitForMeetingsRowCount(page, 1);
    expect(await renderedRowIds(page)).toEqual([recentId]);

    // The attendance window contributes one active constraint to the badge.
    await expect(page.locator(FILTER_BADGE)).toHaveText("1");

    await deleteAllOwnedMeetings(email, name);
  });

  test("sort: changing to 'Meeting id' and toggling direction flips the row order", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-sort-${Date.now()}@videocall.rs`;
    const name = "MfsSortUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // Three owned meetings with lexicographically ordered ids (…_a < …_b < …_c).
    // Stagger the joins so last_active_at is strictly increasing — this lets us
    // assert the "Last active" DEFAULT order independently of the meeting-id
    // sort below.
    const ts = Date.now();
    const idA = `e2e_mfs_sort_${ts}_a`;
    const idB = `e2e_mfs_sort_${ts}_b`;
    const idC = `e2e_mfs_sort_${ts}_c`;
    for (const id of [idA, idB, idC]) {
      await createMeeting(email, name, { meetingId: id, waitingRoomEnabled: false });
      await joinMeeting(email, name, id, name);
      await new Promise((r) => setTimeout(r, 1100));
    }

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 3);

    // Default sort is "Last active" DESC → most-recent join first → c, b, a.
    // This matches the server's unfiltered ordering and confirms the default
    // SortState reproduces today's behaviour.
    expect(await renderedRowIds(page)).toEqual([idC, idB, idA]);

    // Switch the sort key to "Meeting id". Default direction is DESC, so the
    // lexicographic order is reversed → c, b, a (same as above by coincidence
    // of the timestamp ordering, so we don't assert here — we assert after the
    // direction toggle where the orders provably diverge).
    await openSortPopover(page);
    await page.locator(SORT_OPTION, { hasText: "Meeting id" }).click();
    // Selecting an option closes the popover.
    await expect(page.locator(SORT_POPOVER)).toHaveCount(0);
    await expect(page.locator(SORT_TRIGGER)).toContainText("Meeting id");

    // Meeting id DESC → c, b, a.
    expect(await renderedRowIds(page)).toEqual([idC, idB, idA]);

    // Toggle direction to ASC → the order flips to a, b, c.
    await page.locator(SORT_DIR_BTN).click();
    await expect(async () => {
      expect(await renderedRowIds(page)).toEqual([idA, idB, idC]);
    }).toPass({ timeout: 5_000 });

    await deleteAllOwnedMeetings(email, name);
  });

  test("filtered-empty state: a filter that excludes everything shows the message + Clear-filters; clearing restores the list", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-empty-${Date.now()}@videocall.rs`;
    const name = "MfsEmptyUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // A single ACTIVE owned meeting. Filtering on Status = "Ended" excludes it,
    // so the list becomes empty WHILE the feed itself is non-empty — the
    // filtered-empty branch (distinct from the generic "No meetings yet").
    const meetingId = `e2e_mfs_empty_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 1);

    await openFilterPopover(page);
    await checkFilterOption(page, "Ended");

    // The filtered-empty state appears; no rows render. The generic empty
    // ("No meetings yet") must NOT be shown.
    await expect(page.locator(FILTERED_EMPTY)).toBeVisible();
    await expect(page.locator(FILTERED_EMPTY)).toContainText("No meetings match your filters");
    await expect(page.locator(MEETINGS_LIST_ROWS)).toHaveCount(0);
    await expect(page.locator(`${MEETINGS_SECTION} .meetings-empty`)).not.toHaveText(
      "No meetings yet",
    );

    // The Clear-filters button is present; clicking it restores the full list.
    const clearBtn = page.locator(CLEAR_FILTERS_BTN);
    await expect(clearBtn).toBeVisible();
    await clearBtn.click();

    await waitForMeetingsRowCount(page, 1);
    expect(await renderedRowIds(page)).toEqual([meetingId]);
    // Clearing resets all constraints → badge gone.
    await expect(page.locator(FILTER_BADGE)).toHaveCount(0);

    await deleteAllOwnedMeetings(email, name);
  });

  test("persistence: a non-default filter + sort survive a reload", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-persist-${Date.now()}@videocall.rs`;
    const name = "MfsPersistUser";
    await injectSessionCookie(context, { baseURL, email, name });

    // Two meetings so the persisted "I own" filter has something to keep AND
    // something to drop after reload, proving the restored filter is applied,
    // not just stored.
    const ownerEmail = email;
    const ownerName = name;
    const otherEmail = `mfs-persist-other-${Date.now()}@videocall.rs`;
    const otherName = "MfsPersistOther";

    const ownedId = `e2e_mfs_persist_owned_${Date.now()}`;
    await createMeeting(ownerEmail, ownerName, { meetingId: ownedId, waitingRoomEnabled: false });
    await joinMeeting(ownerEmail, ownerName, ownedId, ownerName);

    const guestId = `e2e_mfs_persist_guest_${Date.now()}`;
    await createMeeting(otherEmail, otherName, { meetingId: guestId, waitingRoomEnabled: false });
    await joinMeeting(otherEmail, otherName, guestId, otherName);
    const joinResult = await joinMeeting(ownerEmail, ownerName, guestId, ownerName);
    expect(joinResult.status).toBe("admitted");

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 2);

    // Apply a non-default filter ("I own") and a non-default sort ("Meeting id").
    await openFilterPopover(page);
    await checkFilterOption(page, "I own");
    await waitForMeetingsRowCount(page, 1);

    await openSortPopover(page);
    await page.locator(SORT_OPTION, { hasText: "Meeting id" }).click();
    await expect(page.locator(SORT_TRIGGER)).toContainText("Meeting id");

    // The storage keys are written.
    const storedFilter = await page.evaluate((k) => localStorage.getItem(k), FILTER_STORAGE_KEY);
    const storedSort = await page.evaluate((k) => localStorage.getItem(k), SORT_STORAGE_KEY);
    expect(storedFilter, "filter must persist to localStorage").not.toBeNull();
    expect(storedSort, "sort must persist to localStorage").not.toBeNull();

    // Reload: the selections must be restored AND applied.
    await page.reload();
    await page.waitForTimeout(1500);

    // The filter is restored → only the owned row renders.
    await waitForMeetingsRowCount(page, 1);
    expect(await renderedRowIds(page)).toEqual([ownedId]);

    // The restored sort key is reflected on the trigger and the badge persists.
    await expect(page.locator(SORT_TRIGGER)).toContainText("Meeting id");
    await expect(page.locator(FILTER_BADGE)).toHaveText("1");

    await deleteAllOwnedMeetings(ownerEmail, ownerName);
    await deleteAllOwnedMeetings(otherEmail, otherName);
  });

  test("a11y/interaction: filter popover opens on trigger, closes on Escape and outside-click, returns focus", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `mfs-a11y-${Date.now()}@videocall.rs`;
    const name = "MfsA11yUser";
    await injectSessionCookie(context, { baseURL, email, name });

    const meetingId = `e2e_mfs_a11y_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await joinMeeting(email, name, meetingId, name);

    await page.goto("/");
    await page.waitForTimeout(1500);
    await waitForMeetingsRowCount(page, 1);

    const filterTrigger = page.locator(FILTER_TRIGGER);

    // --- Open on trigger click; aria-expanded flips to true. ---
    await expect(filterTrigger).toHaveAttribute("aria-expanded", "false");
    await openFilterPopover(page);
    await expect(filterTrigger).toHaveAttribute("aria-expanded", "true");

    // --- Escape closes the popover and returns focus to the trigger. ---
    await page.locator(FILTER_POPOVER).press("Escape");
    await expect(page.locator(FILTER_POPOVER)).toHaveCount(0);
    await expect(filterTrigger).toHaveAttribute("aria-expanded", "false");
    await expect(filterTrigger).toBeFocused();

    // --- Re-open, then outside-click (the backdrop) closes it + returns focus. ---
    await openFilterPopover(page);
    await expect(page.locator(POPOVER_BACKDROP)).toBeVisible();
    await page.locator(POPOVER_BACKDROP).click({ position: { x: 2, y: 2 } });
    await expect(page.locator(FILTER_POPOVER)).toHaveCount(0);
    await expect(filterTrigger).toHaveAttribute("aria-expanded", "false");
    await expect(filterTrigger).toBeFocused();

    // --- Opening the sort popover closes the filter popover (mutual exclusion). ---
    await openFilterPopover(page);
    await openSortPopover(page);
    await expect(page.locator(FILTER_POPOVER)).toHaveCount(0);
    await expect(page.locator(SORT_POPOVER)).toBeVisible();

    // Sort popover also closes on Escape and returns focus to the sort trigger.
    await page.locator(SORT_POPOVER).press("Escape");
    await expect(page.locator(SORT_POPOVER)).toHaveCount(0);
    await expect(page.locator(SORT_TRIGGER)).toBeFocused();

    await deleteAllOwnedMeetings(email, name);
  });
});
