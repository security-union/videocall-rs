import { test, expect, Browser, BrowserContext, Page, chromium } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { createMeeting, endMeeting, fetchMeetingState } from "../helpers/meeting-api";

/**
 * E2E coverage for the SSE `feed-changed` → coroutine bridge (issue #1671).
 *
 * ─── What #1671 fixed ───────────────────────────────────────────────────────
 * The home-page meetings list subscribes to the server's live feed-change
 * stream (`GET /api/v1/meetings/feed/stream`, issue #1081) via a browser
 * `EventSource`. The raw `feed-changed` callback fires on the bare browser
 * event-loop stack with NO Dioxus runtime/scope present. Before #1671 that
 * callback called `spawn`/`Signal::set` directly, which PANICS inside
 * dioxus-core (Option::unwrap on the missing runtime, or a RefCell
 * already-borrowed re-entry mid-diff). The panic poisoned the component:
 * the list stopped updating AND its rendered click handlers went dead. The
 * fix routes the nudge through a runtime-free `futures` channel
 * (`notify_feed_changed`) to a `use_coroutine` task that runs INSIDE the
 * component scope, where `meetings.set(...)` is safe (silent in-place update,
 * no loading-spinner flash). See `dioxus-ui/src/components/meetings_list.rs`.
 *
 * ─── Why this reproduces the panic ──────────────────────────────────────────
 * The panic only fires when a REAL `feed-changed` event is delivered to a page
 * that has been mounted long enough for the runtime-free event-loop stack to be
 * the active stack (not the initial mount-fetch stack). So the test:
 *   1. Loads the home page for an observer user O and lets it sit IDLE ~10s
 *      (past the initial mount fetch; the page is now quiescent).
 *   2. Has a SECOND real browser join an unrelated meeting B over WebTransport,
 *      which flips B idle→active server-side and makes the server emit a genuine
 *      `feed-changed` SSE nudge on O's open stream — the exact production path.
 *
 * ─── Which assertion catches which un-fixed behaviour ───────────────────────
 *  - ASSERTION 1 (badge flips Idle→Active live, with NO `.meetings-loading`
 *    spinner): catches the "badge never updates" half of the bug. On the
 *    un-fixed build the `feed-changed` callback panics before it can refetch,
 *    so the row stays stuck on "Idle" forever (no reload happens in this test).
 *    The no-spinner check also pins the #1671 contract that a live nudge is a
 *    SILENT in-place update, not a blank-to-spinner refetch.
 *  - ASSERTION 2 (the row is still clickable → mirrors id into `#meeting-id` →
 *    Start/Join navigates to `/meeting/{id}`): catches the "dead click" half of
 *    the bug. The pre-fix panic poisons the mounted component, so the row's
 *    onclick handler no longer fires — the id never mirrors into the input and
 *    no navigation occurs. Both halves FAIL on the un-fixed (panicking) build,
 *    which is the FAILS-on-unfixed-code requirement.
 *
 * ─── Establishing the REAL nudge (mirrors meeting-idle-active-state.spec.ts) ─
 * A REST join does not increment the actix-api in-memory room member count, so
 * it would not produce the `active` presence signal that drives the
 * `feed-changed` emission. To fire the genuine nudge we drive an actual
 * meeting-room connection exactly the way the live UI does (real WebTransport
 * join → `#grid-container`), reusing the proven `createPresenceContext` /
 * `enterMeetingRoom` helpers copied verbatim from
 * `meeting-idle-active-state.spec.ts`.
 *
 * ─── Local vs CI ────────────────────────────────────────────────────────────
 * This spec needs a real room connection (Dioxus UI :3001, actix-api QUIC :4433
 * with the dev cert, NATS, meeting-api :8081) — the full compose stack, the same
 * requirement as the real-presence cases in `meeting-idle-active-state.spec.ts`.
 * It is UNTAGGED (no `@bvt0`/`@bvt1`), matching that sibling spec, so it runs
 * under the `dioxus` full-suite project and NOT under per-PR `bvt1` CI.
 */

// Active-transition timeout: the observer must see B flip to Active after the
// second browser's real join fires the nudge. Generous margin for connection
// setup + the NATS round-trip + the SSE delivery + the client debounce. Matches
// the sibling spec's constant.
const ACTIVE_TRANSITION_TIMEOUT = 30_000;

// How long the observer page sits IDLE before the nudge arrives. Past the
// initial mount fetch so the runtime-free event-loop stack is the active stack
// when `feed-changed` is delivered — this is the window that reproduced the
// original #1671 panic.
const OBSERVER_IDLE_MS = 10_000;

/**
 * Build an authenticated context for the real-presence join and seed the
 * display name BEFORE any meeting page boots. Copied from
 * `meeting-idle-active-state.spec.ts`: `createAuthenticatedContext` injects the
 * session cookie + WT dev-cert hash but does NOT seed `vc_display_name`, so we
 * pre-seed it via an init script so the meeting page renders the pre-join
 * Start/Join card rather than an inline display-name prompt.
 */
async function createPresenceContext(
  browser: Browser,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
  await ctx.addInitScript((displayName: string) => {
    localStorage.setItem("vc_display_name", displayName);
  }, name);
  return ctx;
}

/**
 * Drive a real meeting-room join from the UI so actix-api registers streaming
 * presence (flipping the meeting active and firing the `feed-changed` nudge).
 * Copied verbatim from `meeting-idle-active-state.spec.ts::enterMeetingRoom`:
 * races the Start/Join button, the two waiting states, and an auto-joined grid,
 * and uses the proven settle delays around the button click before asserting
 * `#grid-container`.
 */
async function enterMeetingRoom(page: Page, meetingId: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}`);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const arrival = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "button" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (arrival === "waiting") {
    throw new Error(
      `enterMeetingRoom(${meetingId}): unexpectedly landed in the waiting room — ` +
        `the host should auto-admit when waitingRoomEnabled=false`,
    );
  }
  if (arrival === "waiting-for-meeting") {
    throw new Error(
      `enterMeetingRoom(${meetingId}): unexpectedly saw "Waiting for meeting to start" — ` +
        `the host's own join should activate the meeting`,
    );
  }

  if (arrival === "grid") {
    await expect(grid).toBeVisible({ timeout: 30_000 });
    return;
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);

  await expect(grid).toBeVisible({ timeout: 30_000 });
}

// Home-list selectors — taken verbatim from `meetings.spec.ts` /
// `meetings-list-filter-sort.spec.ts` and confirmed against the render in
// `dioxus-ui/src/components/meetings_list.rs`.
const MEETINGS_SECTION = ".meetings-list-container";
const MEETINGS_LIST_ROWS = ".meetings-list-container .meeting-item";
const LOADING_SPINNER = `${MEETINGS_SECTION} .meetings-loading`;

/** Locate the home-list row whose `.meeting-id` is exactly `meetingId`. */
function rowFor(page: Page, meetingId: string) {
  return page.locator(MEETINGS_LIST_ROWS, {
    has: page.locator(".meeting-id", { hasText: meetingId }),
  });
}

test.describe("Meetings feed SSE feed-changed nudge (issue #1671)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("home list live-updates a remote meeting going active and stays interactive after an idle nudge", async ({
    context,
    baseURL,
    page,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";

    // Observer O: watches the home page. Joiner J: a separate identity who owns
    // and joins the UNRELATED meeting B over a real WebTransport connection.
    const observerEmail = `feed-nudge-observer-${Date.now()}@videocall.rs`;
    const observerName = "FeedNudgeObserver";
    const joinerEmail = `feed-nudge-joiner-${Date.now()}@videocall.rs`;
    const joinerName = "FeedNudgeJoiner";

    // Meeting B is created by O so it shows up in O's feed, but it is never
    // joined by O — it stays idle until J's real room join activates it. O
    // owning B is what guarantees B appears in O's `/feed` (creator match),
    // so O's open SSE stream receives the `feed-changed` nudge when B activates.
    const meetingB = `e2e_feed_nudge_${Date.now()}`;
    await createMeeting(observerEmail, observerName, {
      meetingId: meetingB,
      waitingRoomEnabled: false,
    });

    // Confirm B starts idle server-side (never activated) before O loads the page.
    await expect
      .poll(() => fetchMeetingState(observerEmail, observerName, meetingB), { timeout: 10_000 })
      .toBe("idle");

    // O loads the home page (cookie-injected). The list mount-fetches once.
    await injectSessionCookie(context, { baseURL, email: observerEmail, name: observerName });
    await page.goto("/");

    // The mount fetch settles (spinner gone) and B renders as an Idle row.
    await expect(page.locator(LOADING_SPINNER)).toHaveCount(0, { timeout: 15_000 });
    const row = rowFor(page, meetingB);
    await expect(row).toBeVisible({ timeout: 15_000 });
    const badge = row.locator(".meeting-state");
    await expect(badge).toHaveClass(/state-idle/);
    await expect(badge).toHaveText("Idle");

    // ── Let the observer page sit IDLE so the nudge lands on the quiescent,
    //    runtime-free event-loop stack — the window that reproduced the panic. ──
    await page.waitForTimeout(OBSERVER_IDLE_MS);

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    let joinerCtx: BrowserContext | undefined;
    try {
      // ── J joins meeting B over a REAL WebTransport room connection. This
      //    flips B idle→active and makes the server emit a genuine
      //    `feed-changed` nudge on O's open `/feed/stream`. ──
      joinerCtx = await createPresenceContext(browser, joinerEmail, joinerName, uiURL);
      const joinerPage = await joinerCtx.newPage();
      await enterMeetingRoom(joinerPage, meetingB);

      // Server-side sanity: B is now active (observed in O's own feed). This
      // proves the nudge's CAUSE actually happened, independent of the UI.
      await expect
        .poll(() => fetchMeetingState(observerEmail, observerName, meetingB), {
          timeout: ACTIVE_TRANSITION_TIMEOUT,
        })
        .toBe("active");

      // ── ASSERTION 1 — catches the "badge never updates" half of the #1671
      //    panic. With NO page.reload()/goto() since the idle wait, O's home
      //    list must flip B's badge Idle→Active purely from the live nudge.
      //    On the un-fixed build the `feed-changed` callback panics before it
      //    can refetch, so this stays "Idle" and times out. ──
      await expect(badge).toHaveText("Active", { timeout: ACTIVE_TRANSITION_TIMEOUT });
      await expect(badge).toHaveClass(/state-active/);

      // #1671 contract: a live nudge is a SILENT in-place update — it must NOT
      // blank the list to the loading spinner. (Was already absent above; assert
      // again now that the live update has been applied.)
      await expect(page.locator(LOADING_SPINNER)).toHaveCount(0);
    } finally {
      // Tear down the joiner first (drops the real transport), then the browser,
      // mirroring the cleanup order in meeting-idle-active-state.spec.ts.
      if (joinerCtx) {
        await joinerCtx.close();
      }
      await browser.close();
    }

    // ── ASSERTION 2 — catches the "dead click" half of the #1671 panic. After
    //    the idle+nudge cycle the row's onclick must still fire. On the home
    //    page a row click mirrors the meeting id into `#meeting-id` (see
    //    home.rs `on_select_meeting`), then Start/Join navigates to
    //    `/meeting/{id}`. The pre-fix panic poisons the component, so the click
    //    is a no-op: the input never mirrors and no navigation happens. ──
    await row.locator(".meeting-item-content").click();
    await expect(page.locator("#meeting-id")).toHaveValue(meetingB, { timeout: 5_000 });

    await page.getByRole("button", { name: "Start or Join Meeting" }).click();
    await expect(page).toHaveURL(/\/meeting\//, { timeout: 10_000 });

    // Cleanup: O owns B, so O can delete it.
    await endMeeting(observerEmail, observerName, meetingB).catch(() => {
      /* tolerated — best-effort cleanup; the per-run unique ids keep runs isolated */
    });
  });
});
