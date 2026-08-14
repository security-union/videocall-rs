import { test, expect, chromium, Page, Locator } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import {
  BROWSER_ARGS,
  DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT,
  createAuthenticatedContext,
} from "../helpers/auth-context";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

/**
 * Rendered label of the admitted-can-admit settings row, verbatim from
 * `dioxus-ui/src/components/meeting_options_controls.rs:197`:
 *   `span { class: "settings-option-label", "Admitted can admit" }`
 *
 * Keep this in sync with that RSX. The row locator below asserts
 * `toHaveCount(1)`, so a drift between the two is reported against the row
 * filter itself rather than as an "element(s) not found" on the compound
 * toggle locator (issue #2099 — see the comment at that assertion).
 */
const ADMITTED_CAN_ADMIT_LABEL = "Admitted can admit";

async function createMeetingViaApi(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  opts: { allowGuests: boolean; waitingRoomEnabled?: boolean; password?: string },
): Promise<string> {
  const token = generateSessionToken(hostEmail, hostName);
  const url = `${API_URL}/api/v1/meetings`;
  const res = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify({
      meeting_id: meetingId,
      attendees: [],
      allow_guests: opts.allowGuests,
      waiting_room_enabled: opts.waitingRoomEnabled ?? true,
      // Issue 1613: `meeting-api` argon2-hashes this at create time and now
      // verifies it on every non-owner join. Omitted => the meeting has no
      // password and no join is ever prompted, which is what every other test
      // in this file relies on.
      ...(opts.password === undefined ? {} : { password: opts.password }),
    }),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${body}`);
  }
  const json = await res.json();
  return json.result.meeting_id;
}

/**
 * Have the host join and start the meeting so it transitions from "idle" to "active".
 */
async function hostStartsMeeting(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  hostEmail: string,
  hostName: string,
  meetingId: string,
  uiURL: string,
): Promise<{
  hostPage: Page;
  hostContext: Awaited<ReturnType<typeof createAuthenticatedContext>>;
}> {
  const hostContext = await createAuthenticatedContext(browser, hostEmail, hostName, uiURL);
  const hostPage = await hostContext.newPage();

  await hostPage.goto("/");
  await hostPage.waitForTimeout(1500);

  await hostPage.locator("#meeting-id").click();
  await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await hostPage.locator("#username").click();
  await hostPage.locator("#username").fill("");
  await hostPage.locator("#username").pressSequentially(hostName, { delay: 50 });
  await hostPage.waitForTimeout(500);
  await hostPage.locator("#username").press("Enter");
  await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await hostPage.waitForTimeout(1500);

  const joinButton = hostPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await joinButton.waitFor({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await joinButton.click();
  await hostPage.waitForTimeout(3000);
  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

  return { hostPage, hostContext };
}

async function ensureJoinedFromTransition(joinButton: Locator, grid: Locator): Promise<void> {
  if (!(await grid.isVisible())) {
    try {
      await joinButton.click({ timeout: 5_000 });
    } catch (error) {
      if (!(await grid.isVisible())) {
        throw error;
      }
    }
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function createGuestContext(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  uiURL: string,
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  await context.addInitScript(DEFAULT_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
  return context;
}

test.describe("Guest join flow", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("guest sees error when guests are not allowed", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_noallow_${Date.now()}`;
    const hostEmail = "host-noguest@videocall.rs";
    const hostName = "HostNoGuest";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: false,
      });

      // Host must start the meeting for it to become active
      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open the guest join page in a new context (no auth cookie)
      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("TestGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Should see the error state
      await expect(guestPage.getByText("Unable to join")).toBeVisible({ timeout: 15_000 });
      await expect(guestPage.getByText("Guests are not allowed in this meeting")).toBeVisible({
        timeout: 5_000,
      });
      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest sees error for non-existent meeting", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_nonexist_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("LostGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // The API returns 403 GUESTS_NOT_ALLOWED for non-existent meetings
      // (to prevent meeting enumeration). The UI shows the error state.
      await expect(guestPage.getByText("Unable to join")).toBeVisible({ timeout: 15_000 });
      await expect(guestPage.getByText("Return to Home")).toBeVisible({ timeout: 5_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest joins directly when allow_guests=true and no waiting room", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_direct_${Date.now()}`;
    const hostEmail = "host-directguest@videocall.rs";
    const hostName = "HostDirect";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
      });

      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open guest join page
      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("DirectGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Guest should be admitted directly (no waiting room).
      // The AttendantsComponent renders either a "Join Meeting"/"Start Meeting"
      // button or goes straight to the grid.
      const joinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
      const grid = guestPage.locator("#grid-container");

      const guestResult = await waitForVisibleState(
        [
          { name: "join-button", locator: joinButton },
          { name: "grid", locator: grid },
        ],
        20_000,
      );

      if (guestResult === "join-button") {
        await guestPage.waitForTimeout(1000);
        await joinButton.click();
        await guestPage.waitForTimeout(3000);
      }

      await expect(grid).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser.close();
    }
  });

  test("guest observer logs the sticky websocket preference source", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_pref_log_${Date.now()}`;
    const hostEmail = "host-guest-pref-log@videocall.rs";
    const hostName = "HostGuestPrefLog";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      const consoleLines: string[] = [];
      guestPage.on("console", (message) => {
        consoleLines.push(message.text());
      });

      await guestPage.goto("/");
      await guestPage.evaluate(() => {
        localStorage.setItem("vc_transport_preference", "websocket");
        localStorage.setItem("vc_transport_sticky", "true");
      });
      await guestPage.reload();
      await guestPage.goto(`/meeting/${meetingId}/guest`);

      await guestPage.locator("#guest-name").fill("PreferenceGuest");
      await guestPage.locator("#guest-name").press("Enter");
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({ timeout: 20_000 });

      await expect
        .poll(
          () =>
            consoleLines.some(
              (line) =>
                line.includes("Transport preference applied:") &&
                line.includes("pref=websocket") &&
                line.includes("source=sticky"),
            ),
          { timeout: 15_000 },
        )
        .toBe(true);
    } finally {
      await browser.close();
    }
  });

  test("guest enters waiting room, host admits, guest transitions to admitted", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_wr_${Date.now()}`;
    const hostEmail = "host-wrguest@videocall.rs";
    const hostName = "HostWR";

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      const { hostPage } = await hostStartsMeeting(browser1, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Open guest join page in a separate browser
      const guestCtx = await createGuestContext(browser2, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Fill in name and submit
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("WaitingGuest", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await guestPage.locator("#guest-name").press("Enter");

      // Guest should enter the waiting room
      await expect(guestPage.getByText("Waiting to be admitted")).toBeVisible({ timeout: 20_000 });

      // Host admits the guest
      const admitButton = hostPage.getByTitle("Admit").first();
      await expect(admitButton).toBeVisible({ timeout: 20_000 });
      await hostPage.waitForTimeout(1000);
      await admitButton.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);

      // Guest should transition to admitted state
      const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
      const guestGrid = guestPage.locator("#grid-container");

      const postAdmit = await waitForVisibleState(
        [
          { name: "join-button", locator: guestJoinButton },
          { name: "grid", locator: guestGrid },
        ],
        20_000,
      );

      if (postAdmit === "join-button") {
        await guestPage.waitForTimeout(1000);
        await guestJoinButton.click();
        await guestPage.waitForTimeout(3000);
      }

      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("admitted guest gains admit controls live when host enables participants-can-admit", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);

    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_aca_live_${Date.now()}`;
    const hostEmail = "host-guest-aca-live@videocall.rs";
    const hostName = "HostGuestAcaLive";
    const admittedParticipantEmail = "admitted-participant@videocall.rs";
    const admittedParticipantName = "AdmittedGuest";

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const admittedGuestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const waitingGuestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: true,
      });

      const { hostPage } = await hostStartsMeeting(
        hostBrowser,
        hostEmail,
        hostName,
        meetingId,
        uiURL,
      );
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      const admittedGuestCtx = await createAuthenticatedContext(
        admittedGuestBrowser,
        admittedParticipantEmail,
        admittedParticipantName,
        uiURL,
      );
      const admittedGuestPage = await admittedGuestCtx.newPage();
      await admittedGuestPage.goto("/");
      await expect(admittedGuestPage.locator("#meeting-id")).toBeVisible({ timeout: 20_000 });
      await admittedGuestPage.locator("#meeting-id").click();
      await admittedGuestPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
      await admittedGuestPage.locator("#username").click();
      await admittedGuestPage.locator("#username").fill("");
      await admittedGuestPage
        .locator("#username")
        .pressSequentially(admittedParticipantName, { delay: 50 });
      await admittedGuestPage.locator("#username").press("Enter");
      await expect(admittedGuestPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
        timeout: 10_000,
      });
      await expect(admittedGuestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      const hostAdmitAdmittedGuest = hostPage.getByTitle("Admit").first();
      await expect(hostAdmitAdmittedGuest).toBeVisible({ timeout: 20_000 });
      await hostAdmitAdmittedGuest.dispatchEvent("click");

      const admittedGuestJoinButton = admittedGuestPage.getByRole("button", {
        name: /Join Meeting|Start Meeting/,
      });
      const admittedGuestGrid = admittedGuestPage.locator("#grid-container");
      const admittedTransition = await waitForVisibleState(
        [
          { name: "join", locator: admittedGuestJoinButton },
          { name: "grid", locator: admittedGuestGrid },
        ],
        20_000,
      );

      if (admittedTransition === "join") {
        await ensureJoinedFromTransition(admittedGuestJoinButton, admittedGuestGrid);
      }
      await expect(admittedGuestGrid).toBeVisible({ timeout: 15_000 });

      const waitingGuestCtx = await waitingGuestBrowser.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      const waitingGuestPage = await waitingGuestCtx.newPage();
      await waitingGuestPage.goto(`/meeting/${meetingId}/guest`);
      await expect(waitingGuestPage.locator("#guest-name")).toBeVisible({ timeout: 20_000 });
      await waitingGuestPage.locator("#guest-name").click();
      await waitingGuestPage.locator("#guest-name").pressSequentially("WaitingGuest", {
        delay: 50,
      });
      await waitingGuestPage.locator("#guest-name").press("Enter");
      await expect(waitingGuestPage.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      const admittedGuestAdmitButton = admittedGuestPage
        .locator('button[title="Admit"], button.btn-admit')
        .first();
      await expect(admittedGuestAdmitButton).not.toBeVisible({ timeout: 5_000 });

      await hostPage.goto(`/meeting/${meetingId}/settings`);
      await expect(hostPage.getByText("Options")).toBeVisible({ timeout: 10_000 });
      // The rendered label is "Admitted can admit"
      // (dioxus-ui/src/components/meeting_options_controls.rs:197). This filter
      // previously used the retired string "Participants can admit others", which
      // matched zero rows — issue #2099.
      const admittedCanAdmitRow = hostPage
        .locator(".settings-option-row")
        .filter({ hasText: ADMITTED_CAN_ADMIT_LABEL });
      // Presence guard: pin the row count BEFORE measuring the toggle. This is a
      // localisation guard, not a silent-to-loud conversion — the aria-checked
      // assertion below is a NON-negated matcher, so it already fails (rather than
      // vacuously passing) on a zero-match. What it cannot do is say *why*: it
      // reports only `element(s) not found` for the compound
      // `.settings-option-row(filtered) >> button[role="switch"]`, which does not
      // distinguish a drifted row label from changed toggle markup. This assertion
      // reports `Expected: 1 / Received: 0` against the row filter alone. It also
      // catches an accidental multi-row match, and forecloses the genuinely vacuous
      // failure mode should the assertion below ever be rewritten into a `.not.`
      // form — a negated matcher DOES pass on zero elements.
      await expect(admittedCanAdmitRow).toHaveCount(1, { timeout: 10_000 });
      const admittedCanAdmitToggle = admittedCanAdmitRow.locator('button[role="switch"]');
      await expect(admittedCanAdmitToggle).toHaveAttribute("aria-checked", "false", {
        timeout: 5_000,
      });
      await admittedCanAdmitToggle.click();
      await expect(admittedCanAdmitToggle).toHaveAttribute("aria-checked", "true", {
        timeout: 5_000,
      });

      await expect(admittedGuestAdmitButton).toBeVisible({ timeout: 60_000 });
      await admittedGuestAdmitButton.dispatchEvent("click");

      const waitingGuestJoinButton = waitingGuestPage.getByRole("button", {
        name: /Join Meeting|Start Meeting/,
      });
      const waitingGuestGrid = waitingGuestPage.locator("#grid-container");
      const waitingTransition = await waitForVisibleState(
        [
          { name: "join", locator: waitingGuestJoinButton },
          { name: "grid", locator: waitingGuestGrid },
        ],
        60_000,
      );

      if (waitingTransition === "join") {
        await ensureJoinedFromTransition(waitingGuestJoinButton, waitingGuestGrid);
      }
      await expect(waitingGuestGrid).toBeVisible({ timeout: 15_000 });
    } finally {
      await hostBrowser.close();
      await admittedGuestBrowser.close();
      await waitingGuestBrowser.close();
    }
  });

  test("join button is disabled when display name is empty", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_empty_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // The guest-name input should start empty
      await expect(guestPage.locator("#guest-name")).toHaveValue("");

      // The submit button should be disabled when the name is empty
      const submitButton = guestPage.locator('button[type="submit"]');
      await expect(submitButton).toBeDisabled();

      // Type a name — button should become enabled
      await guestPage.locator("#guest-name").click();
      await guestPage.locator("#guest-name").pressSequentially("SomeName", { delay: 50 });
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeEnabled();

      // Clear the name — button should be disabled again
      await guestPage.locator("#guest-name").fill("");
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeDisabled();

      // Type only whitespace — button should remain disabled
      await guestPage.locator("#guest-name").fill("   ");
      await guestPage.waitForTimeout(500);
      await expect(submitButton).toBeDisabled();
    } finally {
      await browser.close();
    }
  });

  test("guest join page shows form elements correctly", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_form_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      // Page title
      await expect(guestPage.getByRole("heading", { name: "Join as Guest" })).toBeVisible({
        timeout: 5_000,
      });
      await expect(guestPage.getByRole("heading", { name: "Join Meeting as Guest" })).toBeVisible({
        timeout: 5_000,
      });

      // Meeting ID is displayed
      await expect(guestPage.getByText(meetingId)).toBeVisible({ timeout: 5_000 });

      // Name input with correct label
      await expect(guestPage.getByText("Your Name")).toBeVisible({ timeout: 5_000 });
      await expect(guestPage.locator("#guest-name")).toBeVisible();
      await expect(guestPage.locator("#guest-name")).toHaveAttribute(
        "placeholder",
        "Enter your display name",
      );

      // Validation hint
      await expect(
        guestPage.getByText("Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"),
      ).toBeVisible({ timeout: 5_000 });

      // Guest disclaimer
      await expect(guestPage.getByText("You are joining without an account")).toBeVisible({
        timeout: 5_000,
      });
    } finally {
      await browser.close();
    }
  });

  /**
   * Issue 1613 — the meeting password.
   *
   * `meeting-api` now argon2-verifies a meeting's password on every non-owner
   * join and answers 403 MEETING_PASSWORD_REQUIRED / INVALID_MEETING_PASSWORD.
   * This walks the whole client half of that: the prompt raised by the first
   * 403, a wrong password retried in place, and the correct one getting in.
   *
   * The prompt is driven by the SERVER's 403, not by the meeting's
   * `has_password` flag — which is why this test never reads that flag, and why
   * a guest arriving on a deep link with no listing data still gets prompted.
   */
  test("guest is prompted for the meeting password and can retry in place", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_pw_${Date.now()}`;
    const hostEmail = "host-guestpw@videocall.rs";
    const hostName = "HostGuestPw";
    const MEETING_PASSWORD = "correct-horse-1613";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
        password: MEETING_PASSWORD,
      });

      // The OWNER is exempt server-side (`creator_id`), so the host reaches the
      // grid without ever seeing a prompt. That is asserted here rather than in
      // a separate test because it is the same meeting: if the owner were
      // prompted, this call would hang on the prompt instead of the grid.
      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
      await expect(hostPage.getByTestId("meeting-password-prompt")).toHaveCount(0);

      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      await guestPage.locator("#guest-name").fill("PasswordGuest");
      await guestPage.locator("#guest-name").press("Enter");

      // First 403 (MEETING_PASSWORD_REQUIRED): the prompt appears, and it says
      // "you need one", NOT "you got it wrong" — nothing has been rejected yet.
      const prompt = guestPage.getByTestId("meeting-password-prompt");
      await expect(prompt).toBeVisible({ timeout: 20_000 });
      await expect(guestPage.getByRole("heading", { name: "Password required" })).toBeVisible();
      await expect(guestPage.getByTestId("meeting-password-error")).toHaveCount(0);

      // The field is a real password field and does not render what is typed.
      const field = guestPage.getByTestId("meeting-password-input");
      await expect(field).toHaveAttribute("type", "password");
      // A meeting password is a shared room secret, not a user credential —
      // autofill is suppressed so it never lands in a password manager.
      await expect(field).toHaveAttribute("autocomplete", "off");
      // Focus moves INTO the prompt on open.
      await expect(field).toBeFocused();

      // Second 403 (INVALID_MEETING_PASSWORD): a wrong password is rejected
      // with a DIFFERENT message, in a live region.
      await field.fill("definitely-not-it");
      await guestPage.getByTestId("meeting-password-submit").click();

      const error = guestPage.getByTestId("meeting-password-error");
      await expect(error).toBeVisible({ timeout: 20_000 });
      await expect(error).toHaveAttribute("role", "alert");
      await expect(error).toContainText("That password was incorrect");
      await expect(guestPage.getByRole("heading", { name: "Incorrect password" })).toBeVisible();
      // The error is wired to the field, so moving focus there re-reads it —
      // which is what covers the 2nd and later rejections, where the live
      // region's text has not changed.
      await expect(field).toHaveAttribute("aria-invalid", "true");
      await expect(field).toHaveAttribute("aria-describedby", "meeting-password-error");

      // Retry IN PLACE: the rejected value is dropped and focus is back on the
      // field. Nothing else about the attempt had to be re-entered — the retry
      // below types only the password. (That the display name survives a trip
      // through the prompt is pinned by the cancel test below, where it is
      // directly observable in the form field.)
      await expect(field).toHaveValue("");
      await expect(field).toBeFocused();

      // Third attempt, correct: the guest lands in the meeting.
      await field.fill(MEETING_PASSWORD);
      await guestPage.getByTestId("meeting-password-submit").click();

      const joinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
      const grid = guestPage.locator("#grid-container");
      const guestResult = await waitForVisibleState(
        [
          { name: "join-button", locator: joinButton },
          { name: "grid", locator: grid },
        ],
        25_000,
      );
      if (guestResult === "join-button") {
        await guestPage.waitForTimeout(1000);
        await joinButton.click();
        await guestPage.waitForTimeout(3000);
      }
      await expect(grid).toBeVisible({ timeout: 15_000 });

      // The plaintext is never persisted anywhere the next page load could read.
      const persisted = await guestPage.evaluate((secret) => {
        const scan = (store: Storage) =>
          Object.keys(store).some((key) => (store.getItem(key) ?? "").includes(secret));
        return {
          local: scan(localStorage),
          session: scan(sessionStorage),
          cookie: document.cookie.includes(secret),
          url: window.location.href.includes(secret),
        };
      }, MEETING_PASSWORD);
      expect(persisted).toEqual({ local: false, session: false, cookie: false, url: false });
    } finally {
      await browser.close();
    }
  });

  /**
   * Issue 1613 — the throttled path.
   *
   * After `MAX_FAILED_PASSWORD_ATTEMPTS` (5) failures per `(client IP, meeting)`
   * window, `meeting-api` answers `429 TOO_MANY_PASSWORD_ATTEMPTS` from
   * `consume_attempt` — **before** `verify_offloaded`, so the supplied password
   * is never hashed. Two things follow, and both are asserted below:
   *
   *   1. The user must stay in the prompt. Falling through to the generic error
   *      card would strand them on a Return-to-Home dead end mid-retry.
   *   2. The field must NOT be cleared, and the control must NOT be marked
   *      `aria-invalid`. The server did not look at the value — it may be
   *      exactly right — so discarding it would make the user retype a correct
   *      password once per rejected retry until the window expires.
   *
   * The guest path is the deterministic trigger and is used deliberately:
   * `POST /join-guest` has no rename limiter, so this code is what fires both
   * here and in production. On the AUTHENTICATED path the generic rename
   * limiter runs first and shadows it with `RATE_LIMIT_EXCEEDED` in production
   * — but the e2e stack sets `DISPLAY_NAME_RATE_LIMIT_DISABLED=true` (issue
   * #608), so that shadowing cannot be reproduced here. It is covered instead
   * by the unit test `rename_limiter_is_a_throttle_only_when_a_password_was_supplied`.
   */
  test("the throttled path keeps the user in the prompt and preserves what they typed", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_pw_throttle_${Date.now()}`;
    const hostEmail = "host-guestpwthrottle@videocall.rs";
    const hostName = "HostGuestPwThrottle";
    // `MAX_FAILED_PASSWORD_ATTEMPTS` in meeting-api/src/password.rs. The budget
    // is charged optimistically, so the Nth+1 attempt is the one refused.
    const FAILURE_BUDGET = 5;
    const LAST_TYPED = "still-wrong-but-unchecked";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
        password: "the-real-password",
      });

      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      await guestPage.locator("#guest-name").fill("ThrottledGuest");
      await guestPage.locator("#guest-name").press("Enter");

      const field = guestPage.getByTestId("meeting-password-input");
      const submit = guestPage.getByTestId("meeting-password-submit");
      const error = guestPage.getByTestId("meeting-password-error");
      await expect(guestPage.getByTestId("meeting-password-prompt")).toBeVisible({
        timeout: 20_000,
      });

      // Burn the budget. Each of these is a real verdict, so each clears the
      // field — which is the behaviour the throttled attempt below must NOT
      // share.
      for (let attempt = 1; attempt <= FAILURE_BUDGET; attempt++) {
        await field.fill(`wrong-guess-${attempt}`);
        await submit.click();
        await expect(error).toContainText("That password was incorrect", { timeout: 20_000 });
        await expect(field).toHaveValue("");
      }

      // Budget spent: this one is refused without being verified.
      await field.fill(LAST_TYPED);
      await submit.click();

      await expect(guestPage.getByRole("heading", { name: "Too many attempts" })).toBeVisible({
        timeout: 20_000,
      });
      await expect(error).toContainText("your password was not checked");
      // Still in the prompt, not on the generic "Unable to join" dead end.
      await expect(guestPage.getByTestId("meeting-password-prompt")).toBeVisible();
      await expect(guestPage.getByText("Unable to join")).toHaveCount(0);

      // The two properties that make a throttle different from a verdict.
      await expect(field).toHaveValue(LAST_TYPED);
      await expect(field).toHaveAttribute("aria-invalid", "false");
      // Focus still returns to the field so the retry is one keystroke away.
      await expect(field).toBeFocused();
    } finally {
      await browser.close();
    }
  });

  /**
   * Issue 1613 — backing out of the prompt. The exit has to land somewhere
   * usable AND take focus with it: PR 1756 moved focus in correctly and dropped
   * it to `<body>` on the way out, which was a review blocker.
   */
  test("backing out of the password prompt returns to the guest form with focus", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_guest_pw_cancel_${Date.now()}`;
    const hostEmail = "host-guestpwcancel@videocall.rs";
    const hostName = "HostGuestPwCancel";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeetingViaApi(hostEmail, hostName, meetingId, {
        allowGuests: true,
        waitingRoomEnabled: false,
        password: "some-meeting-password",
      });

      const { hostPage } = await hostStartsMeeting(browser, hostEmail, hostName, meetingId, uiURL);
      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      const guestCtx = await createGuestContext(browser, uiURL);
      const guestPage = await guestCtx.newPage();
      await guestPage.goto(`/meeting/${meetingId}/guest`);
      await guestPage.waitForTimeout(1500);

      await guestPage.locator("#guest-name").fill("CancellingGuest");
      await guestPage.locator("#guest-name").press("Enter");

      await expect(guestPage.getByTestId("meeting-password-prompt")).toBeVisible({
        timeout: 20_000,
      });

      await guestPage.getByRole("button", { name: "Use a different name" }).click();

      // Back on the guest form, with the typed name intact and focus on it.
      await expect(guestPage.getByTestId("meeting-password-prompt")).toHaveCount(0);
      await expect(guestPage.locator("#guest-name")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#guest-name")).toHaveValue("CancellingGuest");
      await expect(guestPage.locator("#guest-name")).toBeFocused();
    } finally {
      await browser.close();
    }
  });
});
