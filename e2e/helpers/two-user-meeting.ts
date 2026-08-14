/**
 * The two-user meeting harness: drive a host and a guest all the way into the
 * same meeting grid, with both able to see each other.
 *
 * Extracted VERBATIM from `tests/two-users-meeting.spec.ts` (where it was
 * introduced for the issue-1884 reaction specs) so a second cross-peer spec —
 * `tests/raise-hand.spec.ts`, issue 2135 — can reuse the exact join dance the
 * existing `@bvt1` test proves, instead of forking a second copy that would
 * drift. `two-users-meeting.spec.ts` now imports from here, so there is one
 * implementation and both specs are fixed together when the join flow changes.
 *
 * Deliberately NOT re-exported through `auth-context.ts`: these two functions
 * are about the *meeting* lifecycle (join / admit / peer-visible), whereas
 * `auth-context.ts` owns *browser* scaffolding (launch args, session cookie).
 */

import { Page, expect } from "@playwright/test";
import { fillAndSubmitJoinForm } from "./join-meeting";

/**
 * From the meeting page, wait for the meeting UI to load and click through
 * "Start Meeting" / "Join Meeting" to enter the grid.
 *
 * The meeting page auto-joins the API when navigated to with a username
 * already set (from the home page). Users who lack a username see an inline
 * display name prompt on the meeting page itself.
 *
 * The auto-join shows a brief "Joining as [name]..." spinner while the API
 * call is in flight. Once the API responds the UI transitions to one of:
 *   - "Ready to join?" with Start/Join Meeting button (admitted)
 *   - "Waiting to be admitted" (waiting room)
 *   - "Waiting for meeting to start" (host hasn't started yet)
 *
 * Auth dropdown (user name/email, sign-out) is only shown on the home
 * page -- it no longer appears on this pre-meeting screen.
 */
export async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
  }

  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

/**
 * First half of the two-user dance: the host starts the meeting and is ALONE in
 * the grid.
 *
 * Split out from `enterTwoUserMeeting` (issue 2135) so a spec can act while the
 * host is still the only participant — the raise-hand suite needs a hand to go
 * up BEFORE anyone else is in the room, to prove a later joiner still learns
 * about it.
 *
 * The split is a pure cut, NOT a rewrite: this body and `guestJoinsMeeting`'s
 * are the old `enterTwoUserMeeting` body line for line, in order, with nothing
 * added or removed, and `enterTwoUserMeeting` is now their concatenation. That
 * matters because `two-users-meeting.spec.ts` is `@bvt1` and calls it. (No grid
 * assertion is added here on purpose: `joinMeetingFromPage` already asserts
 * `#grid-container` visible on BOTH paths that return "in-meeting", so one here
 * would be a tautology — and a redundant line is still a line that did not exist
 * in the function bvt1 is running.)
 */
export async function enterMeetingAsHost(hostPage: Page, meetingId: string): Promise<void> {
  await fillAndSubmitJoinForm(hostPage, meetingId, "HostUser");
  await hostPage.waitForTimeout(1500);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");
}

/**
 * Second half: the guest joins the meeting the host already started, is admitted
 * if the waiting room holds it, and both sides settle with each other's canvas
 * visible (i.e. peer connectivity is established and `on_peer_joined` has fired
 * at the host).
 */
export async function guestJoinsMeeting(
  hostPage: Page,
  guestPage: Page,
  meetingId: string,
): Promise<void> {
  await fillAndSubmitJoinForm(guestPage, meetingId, "GuestUser");
  await guestPage.waitForTimeout(1500);
  const guestResult = await joinMeetingFromPage(guestPage);

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", {
      name: /Join Meeting|Start Meeting/,
    });
    const guestGrid = guestPage.locator("#grid-container");
    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }

  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  // Peer connectivity established (reactions and raised hands ride the same
  // media fan-out).
  await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
}

/**
 * Drive the full two-user join dance (host starts, guest joins + is admitted)
 * and resolve once BOTH participants see the grid and each other's peer canvas.
 * Extracted so the reaction specs (issue 1884) and the raise-hand specs
 * (issue 2135) reuse the exact harness the existing @bvt1 test proves, without
 * a new fixture.
 */
export async function enterTwoUserMeeting(
  hostPage: Page,
  guestPage: Page,
  meetingId: string,
): Promise<void> {
  await enterMeetingAsHost(hostPage, meetingId);
  await guestJoinsMeeting(hostPage, guestPage, meetingId);
}
