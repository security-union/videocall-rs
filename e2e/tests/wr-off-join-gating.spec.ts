/**
 * Issue #2262 — a non-host gated at join time must become un-gated WITHOUT a
 * page reload. One test per lock the fix opens; each "un-fixed, this fails
 * because…" trace lives in the expect() message of the assertion it guards.
 *
 * #2193 cannot make these vacuous: neither asserts a decoded remote camera
 * canvas — (a) stops at `#grid-container`, the same node
 * `guest-waiting-room.spec.ts` asserts, and (b) never enters the grid.
 *
 * Citations below name SYMBOLS, not line numbers, on purpose: a symbol survives
 * an edit above it, a line number does not.
 *
 * UNTAGGED on purpose: (b) sits through a 15s production timer and (a) drives a
 * real observer socket plus a full grid entry, so neither belongs in the `bvt1`
 * project's grep (`playwright.config.ts`). They run only under
 * `--project=dioxus`, so NOT in per-PR CI — the green receipt must come from the
 * local docker stack or a scoped `dioxus` dispatch.
 */

import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import {
  createMeeting,
  endMeeting,
  fetchMeetingState,
  joinMeeting,
  patchMeetingSettings,
} from "../helpers/meeting-api";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

const WAITING_ROOM_CARD = '[data-testid="meeting-waiting-room"]';
const WAITING_FOR_HOST_CARD = '[data-testid="meeting-waiting-for-host"]';
const START_WATCH_INTERVAL_MS = 15_000;

async function navigateToMeetingViaHome(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(
    page,
    "the home page must auto-join into /meeting/{id} with the typed display name " +
      "(same #meeting-id / #username drive guest-waiting-room.spec.ts uses)",
  ).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
}

async function waitForGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await waitForVisibleState(
    [
      { name: "join-button", locator: joinButton },
      { name: "grid", locator: grid },
    ] as const,
    25_000,
  );

  if (result === "join-button") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(
    grid,
    "post-admission the page must reach the in-meeting grid, whether it stopped at prejoin or auto-advanced",
  ).toBeVisible({ timeout: 15_000 });
}

async function readPageAlive(page: Page): Promise<boolean | null> {
  return page.evaluate(
    () => ((window as unknown as Record<string, unknown>).__vc2262_alive as boolean) ?? null,
  );
}

async function markPageAlive(page: Page): Promise<void> {
  await page.evaluate(() => {
    (window as unknown as Record<string, unknown>).__vc2262_alive = true;
  });
  expect(await readPageAlive(page), "sentinel failed to install — later reads prove nothing").toBe(
    true,
  );
}

test.describe("Issue #2262 — join gating clears without a reload", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host turning the waiting room OFF admits a queued non-host in place", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_2262_wroff_${Date.now()}`;
    const hostEmail = "host-2262-wroff@videocall.rs";
    const hostName = "Host2262WrOff";
    const nonHostEmail = "nonhost-2262-wroff@videocall.rs";
    const nonHostName = "NonHost2262WrOff";

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      await createMeeting(hostEmail, hostName, { meetingId, waitingRoomEnabled: true });

      const hostJoin = await joinMeeting(hostEmail, hostName, meetingId, hostName);
      expect(
        hostJoin.status,
        "SANITY FLOOR — the host activates the meeting over the API; the `is_creator` branch of " +
          "`join_meeting` (meeting-api/src/routes/participants.rs) calls `db_meetings::activate`. " +
          "Driving a second browser would add nothing: every assertion below is on the non-host " +
          "page and the toggle is an HTTP PATCH",
      ).toBe("admitted");
      await expect
        .poll(() => fetchMeetingState(hostEmail, hostName, meetingId), {
          timeout: 20_000,
          message:
            "SANITY FLOOR — the meeting must read active before the non-host arrives, or it queues for the wrong reason",
        })
        .toBe("active");

      const context = await createAuthenticatedContext(browser, nonHostEmail, nonHostName, uiURL);
      const page = await context.newPage();

      // Attached before the first navigation: a wasm line logged earlier, or one a reload wipes, is unrecoverable.
      const consoleLines: string[] = [];
      page.on("console", (msg) => consoleLines.push(msg.text()));

      await navigateToMeetingViaHome(page, meetingId, nonHostName);

      const waitingCard = page.locator(WAITING_ROOM_CARD);
      await expect(
        waitingCard,
        "SANITY FLOOR — with the waiting room on and the meeting active the non-host must be queued " +
          "behind the `meeting-waiting-room` card the `WaitingRoom` component renders " +
          "(dioxus-ui/src/components/waiting_room.rs)",
      ).toBeVisible({ timeout: 30_000 });
      await expect(
        page.getByText("Waiting to be admitted"),
        "SANITY FLOOR — the visible card is the queued one, not another card reusing the same container",
      ).toBeVisible({ timeout: 5_000 });

      await expect
        .poll(
          () =>
            consoleLines.some((line) =>
              line.includes("Observer connection established (waiting room)"),
            ),
          {
            timeout: 30_000,
            message:
              "DISCRIMINATOR PRECONDITION — the observer socket must be UP. `WaitingRoom`'s " +
              "`on_connected` is exactly what sets `observer_connected`, and therefore what silenced " +
              "the pre-fix 5s poll; without it, un-fixed code would clear the card by polling and " +
              "this test would prove nothing",
          },
        )
        .toBe(true);

      const windowStart = consoleLines.length;
      await markPageAlive(page);

      // SANITY FLOOR — the helper throws on non-2xx, so a rejected PATCH fails the test here.
      await patchMeetingSettings(hostEmail, hostName, meetingId, { waiting_room_enabled: false });

      await expect(
        waitingCard,
        "DISCRIMINATOR 1 — the queued card must clear with no reload and no click. Un-fixed, two " +
          "independent locks each hold it up forever: (1) no event reaches the client — the per-user " +
          "`publish_participant_admitted` loop over `auto_admitted_user_ids` in `update_meeting` " +
          "(meeting-api/src/routes/meetings.rs) could not have existed before, because the bulk admit " +
          "in `db::meetings::update_meeting_settings` was a plain execute with no RETURNING user_id, " +
          "and the toggle published only MEETING_SETTINGS_UPDATED, which `WaitingRoom` ignores " +
          "(on_waiting_room_updated: None / on_meeting_settings_updated: None), leaving its " +
          "`on_participant_admitted` callback as the only admission trigger; (2) polling was " +
          "suppressed — `WaitingRoom`'s 5s poll closure now calls `should_poll(observer_connected, " +
          "tick)` where it previously read `if observer_connected.get() { return; }`, so with the " +
          "socket UP the 5s safety net issued no HTTP poll at all. The DB row is admitted either way, " +
          "which is the incident's 'only a refresh got me in'",
      ).toBeHidden({ timeout: 30_000 });

      await expect
        .poll(
          () =>
            consoleLines
              .slice(windowStart)
              .some((line) => line.includes("Received PARTICIPANT_ADMITTED:")),
          {
            timeout: 10_000,
            message:
              "DISCRIMINATOR 2 (wire level) — the packet the new publish loop emits must actually " +
              "arrive; logged by the PARTICIPANT_ADMITTED arm of `Inner::on_inbound_media` " +
              "(videocall-client/src/client/video_call_client.rs)",
          },
        )
        .toBe(true);
      const admitLine = consoleLines
        .slice(windowStart)
        .find((line) => line.includes("Received PARTICIPANT_ADMITTED:"));
      expect(admitLine, "PARTICIPANT_ADMITTED line disappeared after the poll saw it").toBeTruthy();
      expect(admitLine, "the admitted packet must name THIS room").toContain(`room=${meetingId}`);
      expect(
        admitLine,
        "the packet must be addressed to THIS user — that arm's " +
          "`target_user_id == options.user_id` filter matches because `update_meeting` passes each " +
          "`admitted_user_id` (the DB user_id) to `publish_participant_admitted`, and for e2e session " +
          "tokens that id is the email (`generateSessionToken` in helpers/auth.ts sets `sub: email`)",
      ).toContain(`target=${nonHostEmail}`);

      // Judged on the LAST lifecycle line in the window, not on "no lost line at all", so a drop that
      // reconnects before the transition does not void a legitimate run.
      const lifecycle = consoleLines
        .slice(windowStart)
        .filter(
          (line) =>
            line.includes("Observer connection established (waiting room)") ||
            line.includes("Observer connection lost (waiting room)"),
        );
      const downAtTransition =
        lifecycle.length > 0 && lifecycle[lifecycle.length - 1].includes("lost");
      expect(
        downAtTransition,
        "VOIDS THE RUN — the observer socket was DOWN at the transition (`WaitingRoom`'s " +
          "`on_connected` / `on_connection_lost` emit these two lines). Down, observer_connected is " +
          "false and the pre-fix 5s poll would have cleared the card too. An empty lifecycle list is " +
          "fine: it means nothing changed since the precondition saw it come up",
      ).toBe(false);

      expect(
        await readPageAlive(page),
        "DISCRIMINATOR 3 — same document throughout. Null means a fresh document wiped the sentinel, " +
          "i.e. this was a refresh, and the fix is about NOT reloading",
      ).toBe(true);

      await waitForGrid(page);
      await expect(
        page.locator("#grid-container"),
        "DISCRIMINATOR 4 — the admitted participant must actually land in the meeting",
      ).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("start watch re-joins a page parked on 'Waiting for meeting to start'", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_2262_startwatch_${Date.now()}`;
    const hostEmail = "host-2262-startwatch@videocall.rs";
    const hostName = "Host2262StartWatch";
    const nonHostEmail = "nonhost-2262-startwatch@videocall.rs";
    const nonHostName = "NonHost2262StartWatch";

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      await createMeeting(hostEmail, hostName, { meetingId, waitingRoomEnabled: true });
      await endMeeting(hostEmail, hostName, meetingId);

      const context = await createAuthenticatedContext(browser, nonHostEmail, nonHostName, uiURL);
      const page = await context.newPage();

      const consoleLines: string[] = [];
      page.on("console", (msg) => consoleLines.push(msg.text()));

      await navigateToMeetingViaHome(page, meetingId, nonHostName);

      const waitingForHost = page.locator(WAITING_FOR_HOST_CARD);
      await expect(
        waitingForHost,
        "SANITY FLOOR — the page must park in the EMPTY-observer-token WaitingForMeeting branch (the " +
          "`Err(JoinError::MeetingNotActive)` arm in dioxus-ui/src/pages/meeting.rs), reached by " +
          "joining an ENDED meeting: `db::meetings::create_with_options` INSERTs state='idle' and " +
          "`db::meetings::end_meeting` guards only on `state <> 'ended'`, so the meeting really does " +
          "end; the join is then refused 400 MEETING_NOT_ACTIVE by `join_as_attendee`'s " +
          '`current_state == "ended"` guard, which `parse_api_response` ' +
          "(videocall-meeting-client/src/lib.rs) maps to ApiError::MeetingNotActive. On an empty token " +
          "the observer `use_effect` early-returns and builds NO client at all",
      ).toBeVisible({ timeout: 30_000 });
      await expect(
        page.getByText("Waiting for meeting to start"),
        "SANITY FLOOR — the visible card is the pre-activation `meeting-waiting-for-host` one, not the queued card",
      ).toBeVisible({ timeout: 5_000 });

      await markPageAlive(page);
      const windowStart = consoleLines.length;

      const hostJoin = await joinMeeting(hostEmail, hostName, meetingId, hostName);
      expect(
        hostJoin.status,
        "SANITY FLOOR — the host really did activate the meeting, so a red result below cannot be " +
          "misread as 'the host never started'",
      ).toBe("admitted");
      await expect
        .poll(() => fetchMeetingState(hostEmail, hostName, meetingId), {
          timeout: 20_000,
          message:
            "SANITY FLOOR — `db::meetings::display_state` returns active only once an admitted " +
            "participant exists (it is fed the `db_participants::count_admitted` result by " +
            "`get_meeting`), so nothing could have escaped early on a false positive",
        })
        .toBe("active");

      const waitingCard = page.locator(WAITING_ROOM_CARD);
      await expect(
        waitingCard,
        "DISCRIMINATOR 1 — within two poll periods plus slack (the local constant mirrors " +
          "`START_WATCH_INTERVAL_MS` in dioxus-ui/src/components/waiting_room.rs) the page must " +
          "re-join on its own and be REPLACED by the queued card (the waiting room is on, so the " +
          "re-join answers `waiting`). The whole escape is the start-watch `use_future` in " +
          "dioxus-ui/src/pages/meeting.rs — the ONLY timer in that file (its `use_future` and the " +
          "`TimeoutFuture` inside it are that file's only occurrences of either), and its poller reads " +
          "GET /api/v1/meetings/{id}, whose handler `get_meeting` takes only `AuthUser` with no " +
          "participation or ownership check and so answers for a user with no participant row. " +
          "Un-fixed there is no timer in this state and no observer client to push to it, so the page " +
          "sits here until someone reloads it",
      ).toBeVisible({ timeout: 2 * START_WATCH_INTERVAL_MS + 15_000 });
      await expect(
        waitingForHost,
        "DISCRIMINATOR 1 (cont.) — the pre-activation card must be gone, not merely stacked behind the queued one",
      ).toBeHidden({ timeout: 5_000 });

      await expect
        .poll(
          () =>
            consoleLines
              .slice(windowStart)
              .some((line) => line.includes("Start watch: meeting is active, re-joining")),
          {
            timeout: 10_000,
            message:
              "DISCRIMINATOR 2 — the re-join must come from the new poller specifically: this line is " +
              "emitted only inside the start-watch `use_future` in dioxus-ui/src/pages/meeting.rs. " +
              "Delete that block and this goes red, which is the 'can be deleted wholesale and " +
              "nothing reds' blocker",
          },
        )
        .toBe(true);

      const escapeIdx = consoleLines.findIndex((line) =>
        line.includes("Start watch: meeting is active, re-joining"),
      );
      expect(escapeIdx, "start-watch line vanished after the poll saw it").toBeGreaterThanOrEqual(
        0,
      );
      // Scoped to lines BEFORE the escape on purpose: the re-join answers `waiting`, whose arm sets a
      // non-empty observer_token_signal, so the same page-level effect legitimately connects — and
      // logs — a second later. An unscoped check would go red on FIXED code.
      const beforeEscape = consoleLines.slice(0, escapeIdx);
      expect(
        beforeEscape.filter((line) => line.includes("Meeting activated push received")),
        "DISCRIMINATOR 3 — a push escaped the state: on this signed-in page the only emitter of this " +
          "line is the WaitingForMeeting observer's `on_meeting_activated` in " +
          "dioxus-ui/src/pages/meeting.rs, so the run exercised the push, not the poller",
      ).toEqual([]);
      expect(
        beforeEscape.filter((line) =>
          line.includes("Observer connection established (waiting for meeting)"),
        ),
        "DISCRIMINATOR 3 (cont.) — an observer client existed pre-escape (this line is logged only " +
          "from the WaitingForMeeting observer's `on_connected`), so the empty-token precondition did " +
          "not hold and the push escape was available after all",
      ).toEqual([]);

      expect(
        await readPageAlive(page),
        "DISCRIMINATOR 4 — same document throughout. Null means a fresh document wiped the sentinel, " +
          "i.e. this was a refresh, and the fix is about NOT reloading",
      ).toBe(true);
    } finally {
      await browser.close();
    }
  });
});
