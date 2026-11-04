import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { openPeerList } from "../helpers/controls";
import { enterMeetingAsHost, guestJoinsMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 3123: the participant panel's Sort control orders the remote roster
 * rows, keeps the self row first, persists per browser, and never reorders
 * the video grid. The join order below is chosen so that Name, Join order and
 * Recently joined each give a different row order.
 */

const OBSERVER = "SortObserver";
const JOIN_ORDER = ["mila", "Zane", "Ava"];
const BY_NAME = ["Ava", "mila", "Zane"];
const BY_RECENT = ["Ava", "Zane", "mila"];
const OPTION_LABELS = [
  "Name",
  "Join order",
  "Recently joined",
  "Role",
  "Raised hands",
  "Unmuted",
  "Camera on",
];

const SORT =
  '#peer-list-container .in-call-header select#peer-list-sort[data-testid="peer-list-sort"]';
const ROWS = "#peer-list-container .peer-list > ul > li";
const REMOTE_NAMES = `${ROWS}:nth-child(n+2) .peer_item_name_container`;
const GRID_NAMES = '#grid-container [data-tile-root="true"] h4.floating-name .floating-name-text';

async function chooseSort(page: Page, label: string): Promise<void> {
  const sort = page.locator(SORT);
  await sort.hover();
  await sort.selectOption({ label });
}

test.describe("Participant list sort (issue 3123)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("Sort reorders the remote roster rows, persists, and leaves the grid alone", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_roster_sort_${Date.now()}`;
    const browsers: Browser[] = [];
    const launch = async (name: string): Promise<Page> => {
      const browser = await chromium.launch({ args: BROWSER_ARGS });
      browsers.push(browser);
      const email = `roster-sort-${name.toLowerCase()}@videocall.rs`;
      const context = await createAuthenticatedContext(browser, email, name, uiURL);
      return context.newPage();
    };

    try {
      const observer = await launch(OBSERVER);
      await enterMeetingAsHost(observer, meetingId, OBSERVER);
      const gridNames = observer.locator(GRID_NAMES);
      // Join time is when the observer first sees a peer, so each joiner must
      // be on the observer's grid before the next one starts.
      for (const name of JOIN_ORDER) {
        await guestJoinsMeeting(observer, await launch(name), meetingId, name);
        await expect(gridNames.filter({ hasText: new RegExp(`^${name}$`) })).toHaveCount(1, {
          timeout: 30_000,
        });
      }
      await expect(gridNames).toHaveCount(JOIN_ORDER.length);
      const gridOrder = await gridNames.allTextContents();

      await openPeerList(observer);
      const sort = observer.locator(SORT);
      await expect(sort).toHaveCount(1);
      await expect(sort).toHaveAttribute("aria-label", "Sort participants");
      await expect(sort.locator("option")).toHaveText(OPTION_LABELS);
      await expect(sort).toHaveValue("name");

      const rows = observer.locator(ROWS);
      const remoteNames = observer.locator(REMOTE_NAMES);
      await expect(rows).toHaveCount(JOIN_ORDER.length + 1);
      await expect(rows.first()).toContainText(OBSERVER);
      await expect(remoteNames).toHaveText(BY_NAME);

      await chooseSort(observer, "Join order");
      await expect(remoteNames).toHaveText(JOIN_ORDER);

      await chooseSort(observer, "Recently joined");
      await expect(remoteNames).toHaveText(BY_RECENT);
      await expect(rows.first()).toContainText(OBSERVER);
      await expect(gridNames).toHaveText(gridOrder);
      expect(await observer.evaluate(() => localStorage.getItem("vc_peer_list_sort"))).toBe(
        '"recent"',
      );

      await observer.locator("#peer-list-container .sidebar-header .close-button").click();
      await expect(observer.locator("#peer-list-container")).not.toHaveClass(/\bvisible\b/);
      await expect(sort).toHaveCount(0);
      await openPeerList(observer);
      await expect(sort).toHaveValue("recent");
      await expect(remoteNames).toHaveText(BY_RECENT);
      await expect(rows.first()).toContainText(OBSERVER);
    } finally {
      await Promise.all(browsers.map((b) => b.close()));
    }
  });
});
