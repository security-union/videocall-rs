import { test, expect, Page, chromium } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { createMeeting, joinMeeting, endMeeting, fetchMeetingState } from "../helpers/meeting-api";

/**
 * E2E coverage for the presence-driven meeting idle/active state machine
 * (branch `feat/meeting-idle-state`).
 *
 * ─── Behaviour under test (server-side) ─────────────────────────────────────
 * A meeting's `state` ∈ {idle, active, ended} now reflects live presence:
 *   - Created-but-unjoined (or fully drained) → `idle`.
 *   - A participant joins / is present         → `active`.
 *   - The LAST present participant leaves (and the meeting has NOT ended)
 *     → `idle` again.
 *   - Ending (host leave with end_on_host_leave, or explicit /end) is the
 *     terminal `ended` state and is NOT flipped back to idle by a later
 *     disconnect.
 *
 * Mechanism (for context — not directly asserted): actix-api detects a room's
 * in-memory member count reaching zero and publishes NATS
 * `internal.meeting_became_empty`; meeting-api consumes it and runs
 * `db_meetings::set_idle` (guarded on `state='active'`, so it never overwrites
 * `ended` and is idempotent). Join/admit calls `db_meetings::activate()`. The
 * empty→idle transition is therefore ASYNCHRONOUS: it only fires after the
 * real transport (WebTransport/WebSocket) connection drops and the NATS event
 * round-trips. Every state assertion below uses `expect.poll` with a generous
 * timeout — we assert the EVENTUAL state, never instantaneous timing.
 *
 * ─── How state is observed ──────────────────────────────────────────────────
 * Primary, deterministic signal: `GET /api/v1/meetings/feed` via the
 * `fetchMeetingState` helper, which returns the row's `state` string. This is
 * the same server-side source of truth that backs the home-page list and is
 * polled directly so the assertions don't depend on UI re-render timing.
 *
 * The home page ALSO renders a visible per-row badge — `.meeting-state` with a
 * `state-idle` / `state-active` / `state-ended` modifier class and the label
 * "Idle" / "Active" / "Ended" (see `dioxus-ui/src/components/meetings_list.rs`
 * + `meeting_format.rs`). The created=idle case additionally asserts that badge
 * so the UI binding is covered at least once; the async transitions are asserted
 * via the feed (deterministic) rather than racing the home-page poll interval.
 *
 * ─── Establishing + dropping REAL streaming presence ────────────────────────
 * A REST `joinMeeting` only writes a `meeting_participants` row — it does NOT
 * increment the actix-api in-memory room member count, so it neither produces
 * the `active` presence signal that everyone-left depends on, nor the
 * disconnect that triggers `meeting_became_empty`. To exercise the real
 * presence machine we drive an actual meeting-room connection exactly the way
 * the live UI does (mirroring `two-users-meeting.spec.ts`), using the shared
 * `createAuthenticatedContext` helper (which injects BOTH the session cookie
 * AND the WebTransport dev-cert hash the wasm client needs for the QUIC
 * handshake — see `helpers/auth-context.ts`):
 *   - JOIN  → open `/meeting/{id}` in a real browser context, click
 *             "Start/Join Meeting", and wait for `#grid-container`. Reaching the
 *             grid means the wasm client opened its transport to actix-api and
 *             the room member count incremented → meeting flips to `active`.
 *   - LEAVE → close the page (and its context). Closing the page tears down the
 *             transport; actix-api observes the member count reach zero and
 *             publishes `meeting_became_empty` → meeting flips back to `idle`.
 *
 * ─── Local vs CI ────────────────────────────────────────────────────────────
 * The created=idle and ended-stays-ended (REST) cases run anywhere the
 * meeting-api is reachable. The join→active, everyone-leaves→idle, rejoin→active
 * and ended-survives-disconnect cases require a real room connection, which
 * needs the full compose stack (Dioxus UI :3001, actix-api QUIC :4433 with the
 * dev cert, NATS, meeting-api :8081). Those run in CI / `make e2e`; a laptop
 * without the Docker stack up will time out at `#grid-container` — the expected
 * harness gap, not a test bug.
 */

// Generous timeout for the empty→idle transition: transport teardown followed
// by a NATS round-trip and a DB write, observed by polling the feed.
const IDLE_TRANSITION_TIMEOUT = 30_000;
// Join→active is faster (a synchronous activate() on admit) but still async to
// the observer; keep a comfortable margin for connection setup under load.
const ACTIVE_TRANSITION_TIMEOUT = 30_000;

/**
 * Drive a real meeting-room join from the UI so actix-api registers streaming
 * presence. Navigates to `/meeting/{id}`, clicks through the pre-join
 * Start/Join button, and resolves once `#grid-container` is visible (the marker
 * that the in-meeting transport is up). Auto-join (grid appears without a
 * button) is handled too.
 */
async function enterMeetingRoom(page: Page, meetingId: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}`);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const arrival = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "button" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
  ]);

  if (arrival === "button") {
    await joinButton.click();
    await expect(grid).toBeVisible({ timeout: 30_000 });
  }
}

test.describe("Meeting idle/active presence-driven state transitions", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("created-but-unjoined meeting is idle (feed + home-page badge)", async ({
    context,
    baseURL,
    page,
  }) => {
    const email = `idle-created-${Date.now()}@videocall.rs`;
    const name = "IdleCreatedUser";

    const meetingId = `e2e_idle_created_${Date.now()}`;
    // Create only — nobody joins, so no presence is ever established.
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });

    // Feed is the deterministic source of truth: a never-activated meeting is idle.
    await expect
      .poll(() => fetchMeetingState(email, name, meetingId), { timeout: 10_000 })
      .toBe("idle");

    // The home page renders a visible "Idle" badge for that row.
    await injectSessionCookie(context, { baseURL, email, name });
    await page.goto("/");

    const row = page.locator(".meetings-list-container .meeting-item", {
      has: page.locator(".meeting-id", { hasText: meetingId }),
    });
    await expect(row).toBeVisible({ timeout: 15_000 });
    const badge = row.locator(".meeting-state");
    await expect(badge).toHaveClass(/state-idle/);
    await expect(badge).toHaveText("Idle");
  });

  test("ended meeting stays ended and a later REST re-join does NOT flip it to idle", async () => {
    // REST-driven (no room connection needed) so it runs anywhere the
    // meeting-api is reachable. Locks the terminal-state invariant: the
    // `set_idle` guard (`WHERE state='active'`) must never resurrect an `ended`
    // meeting into `idle`.
    const email = `ended-terminal-${Date.now()}@videocall.rs`;
    const name = "EndedTerminalUser";

    const meetingId = `e2e_ended_terminal_${Date.now()}`;
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });

    // Owner REST-join activates the meeting, then we end it explicitly.
    await joinMeeting(email, name, meetingId, name);
    await endMeeting(email, name, meetingId);

    await expect
      .poll(() => fetchMeetingState(email, name, meetingId), { timeout: 10_000 })
      .toBe("ended");

    // A subsequent join attempt must not silently resurrect the meeting. The
    // server may reject joining an ended meeting; the invariant we assert is the
    // observed state, not the join result.
    await joinMeeting(email, name, meetingId, name).catch(() => {
      /* tolerated — the state invariant below is the load-bearing assertion */
    });

    // Hold the line: poll repeatedly and assert it is never "idle".
    const deadline = Date.now() + 8_000;
    while (Date.now() < deadline) {
      const state = await fetchMeetingState(email, name, meetingId);
      expect(state, "an ended meeting must never transition to idle").not.toBe("idle");
      await new Promise((r) => setTimeout(r, 500));
    }
  });

  test("join → active, everyone leaves → idle, rejoin → active (real streaming presence)", async ({
    baseURL,
  }) => {
    // The core new behaviour. Requires a REAL room connection (UI → actix-api
    // transport) to drive presence to one and back to zero — a REST-only join
    // would never trigger the empty→idle path and would be false confidence.
    const uiURL = baseURL || "http://localhost:3001";
    const email = `presence-cycle-${Date.now()}@videocall.rs`;
    const name = "PresenceCycleUser";
    const meetingId = `e2e_presence_cycle_${Date.now()}`;

    // Create the meeting (owner). It starts idle (never activated).
    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
    await expect
      .poll(() => fetchMeetingState(email, name, meetingId), { timeout: 10_000 })
      .toBe("idle");

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      // ── 1) Join → active ──────────────────────────────────────────────────
      let ctx = await createAuthenticatedContext(browser, email, name, uiURL);
      let page = await ctx.newPage();
      await enterMeetingRoom(page, meetingId);

      await expect
        .poll(() => fetchMeetingState(email, name, meetingId), {
          timeout: ACTIVE_TRANSITION_TIMEOUT,
        })
        .toBe("active");

      // ── 2) Everyone leaves → idle ─────────────────────────────────────────
      // Close the page + context: the transport drops, actix-api sees the room
      // member count reach zero, publishes meeting_became_empty, and meeting-api
      // sets the meeting idle. Asynchronous — poll with a generous timeout.
      await page.close();
      await ctx.close();

      await expect
        .poll(() => fetchMeetingState(email, name, meetingId), {
          timeout: IDLE_TRANSITION_TIMEOUT,
        })
        .toBe("idle");

      // ── 3) Rejoin idle → active ───────────────────────────────────────────
      ctx = await createAuthenticatedContext(browser, email, name, uiURL);
      page = await ctx.newPage();
      await enterMeetingRoom(page, meetingId);

      await expect
        .poll(() => fetchMeetingState(email, name, meetingId), {
          timeout: ACTIVE_TRANSITION_TIMEOUT,
        })
        .toBe("active");

      await page.close();
      await ctx.close();
    } finally {
      await browser.close();
    }
  });

  test("ended while a participant is present stays ended after they disconnect (real presence)", async ({
    baseURL,
  }) => {
    // End-vs-idle race, the realistic ordering: a participant is genuinely
    // present (active via real transport), the owner ends the meeting, THEN the
    // participant disconnects. The disconnect's empty→idle event must lose to
    // the terminal `ended` state (set_idle's `WHERE state='active'` guard).
    const uiURL = baseURL || "http://localhost:3001";
    const email = `ended-present-${Date.now()}@videocall.rs`;
    const name = "EndedPresentUser";
    const meetingId = `e2e_ended_present_${Date.now()}`;

    await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx = await createAuthenticatedContext(browser, email, name, uiURL);
      const page = await ctx.newPage();
      await enterMeetingRoom(page, meetingId);

      // Confirm real presence brought it to active.
      await expect
        .poll(() => fetchMeetingState(email, name, meetingId), {
          timeout: ACTIVE_TRANSITION_TIMEOUT,
        })
        .toBe("active");

      // End it while the participant is still connected.
      await endMeeting(email, name, meetingId);
      await expect
        .poll(() => fetchMeetingState(email, name, meetingId), { timeout: 10_000 })
        .toBe("ended");

      // Now disconnect: the empty event fires, but ended must win.
      await page.close();
      await ctx.close();

      // Hold the line across the full idle-transition window: state must stay
      // "ended" and never flip to "idle".
      const deadline = Date.now() + IDLE_TRANSITION_TIMEOUT;
      while (Date.now() < deadline) {
        const state = await fetchMeetingState(email, name, meetingId);
        expect(state, "ended must survive a post-end disconnect").toBe("ended");
        await new Promise((r) => setTimeout(r, 1_000));
      }
    } finally {
      await browser.close();
    }
  });
});
