/**
 * Issue 1613 — the meeting password on the AUTHENTICATED join flow
 * (`dioxus-ui/src/pages/meeting.rs`).
 *
 * `meeting-api` argon2-verifies a meeting's password on every non-owner join
 * path and answers `403 MEETING_PASSWORD_REQUIRED` (protected, none supplied)
 * or `403 INVALID_MEETING_PASSWORD` (wrong). The client half turns those two
 * codes into a prompt.
 *
 * The guest half of the same feature lives in `guest-join.spec.ts`, which owns
 * the unauthenticated `/meeting/{id}/guest` layout. This file covers the other
 * page because the two differ in exactly the places that matter here: the
 * meeting page AUTO-joins (so the prompt replaces a spinner, not a form) and
 * its back-out navigates home rather than returning to a form.
 *
 * The prompt is driven by the SERVER's 403, not by the meeting's `has_password`
 * flag — so nothing in this file reads that flag, and an attendee who arrives
 * on a deep link with no meeting-listing data still gets prompted.
 */
import { test, expect } from "@playwright/test";
import { launchAuthenticatedBrowser } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { createMeeting } from "../helpers/meeting-api";
import { enterMeetingAsHost, joinMeetingFromPage } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

const OWNER_EMAIL = "owner-1613@videocall.rs";
const OWNER_NAME = "PasswordOwner";
const ATTENDEE_EMAIL = "attendee-1613@videocall.rs";
const ATTENDEE_NAME = "PasswordAttendee";

test.describe("Meeting password — authenticated join (issue 1613)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // `@bvt1` so per-PR CI runs it: this is the client half of an access control,
  // and a regression that silently stopped raising the prompt would otherwise
  // only be caught by the full `dioxus` suite. The harness (two authenticated
  // browsers into one meeting) is the same shape `two-users-meeting.spec.ts`
  // already runs under `@bvt1`, so it is not new risk for that runner. The
  // guest-side specs in `guest-join.spec.ts` stay untagged, matching that file.
  test("attendee is prompted for the meeting password and can retry in place @bvt1", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_pw_auth_${Date.now()}`;
    const MEETING_PASSWORD = "correct-horse-1613";

    // `launchAuthenticatedBrowser` already applies BROWSER_ARGS; the 4th
    // parameter is for EXTRA args, and this suite needs none.
    const owner = await launchAuthenticatedBrowser(OWNER_EMAIL, OWNER_NAME, uiURL);
    const attendee = await launchAuthenticatedBrowser(ATTENDEE_EMAIL, ATTENDEE_NAME, uiURL);

    try {
      // The creator of this meeting is OWNER_EMAIL — the same identity the
      // `owner` browser carries — so `creator_id` matches and the server exempts
      // them from the password check.
      await createMeeting(OWNER_EMAIL, OWNER_NAME, {
        meetingId,
        waitingRoomEnabled: false,
        password: MEETING_PASSWORD,
      });

      // Owner exemption: reaching the grid IS the assertion. `enterMeetingAsHost`
      // waits for the join button / grid, so a prompt here would time it out
      // rather than pass silently. The explicit count assertion pins it anyway.
      const ownerPage = owner.page;
      await enterMeetingAsHost(ownerPage, meetingId);
      await expect(ownerPage.getByTestId("meeting-password-prompt")).toHaveCount(0);

      // Non-owner attendee: the meeting page auto-joins on arrival, so the 403
      // lands with no interaction beyond submitting the home form.
      const attendeePage = attendee.page;
      await fillAndSubmitJoinForm(attendeePage, meetingId, ATTENDEE_NAME);

      // First 403 (MEETING_PASSWORD_REQUIRED) — "you need one", NOT "you got it
      // wrong": nothing the user typed has been rejected yet.
      const prompt = attendeePage.getByTestId("meeting-password-prompt");
      await expect(prompt).toBeVisible({ timeout: 30_000 });
      await expect(attendeePage.getByRole("heading", { name: "Password required" })).toBeVisible();
      await expect(attendeePage.getByTestId("meeting-password-error")).toHaveCount(0);

      const field = attendeePage.getByTestId("meeting-password-input");
      await expect(field).toHaveAttribute("type", "password");
      await expect(field).toHaveAttribute("autocomplete", "off");
      // Focus moves INTO the prompt on open.
      await expect(field).toBeFocused();

      // Second 403 (INVALID_MEETING_PASSWORD) — a different message, in a live
      // region, wired to the field so moving focus there re-reads it.
      await field.fill("definitely-not-it");
      await attendeePage.getByTestId("meeting-password-submit").click();

      const error = attendeePage.getByTestId("meeting-password-error");
      await expect(error).toBeVisible({ timeout: 20_000 });
      await expect(error).toHaveAttribute("role", "alert");
      await expect(error).toContainText("That password was incorrect");
      await expect(attendeePage.getByRole("heading", { name: "Incorrect password" })).toBeVisible();
      await expect(field).toHaveAttribute("aria-invalid", "true");
      await expect(field).toHaveAttribute("aria-describedby", "meeting-password-error");

      // Retry IN PLACE: the rejected value is dropped and focus returns to the
      // field. The meeting context is untouched — the retry re-issues the same
      // join, so nothing but the password is typed again.
      await expect(field).toHaveValue("");
      await expect(field).toBeFocused();

      // Third attempt, correct: straight into the meeting.
      await field.fill(MEETING_PASSWORD);
      await attendeePage.getByTestId("meeting-password-submit").click();

      const attendeeResult = await joinMeetingFromPage(attendeePage);
      expect(attendeeResult).toBe("in-meeting");

      // The plaintext is never persisted anywhere a later page load could read.
      const persisted = await attendeePage.evaluate((secret) => {
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
      await attendee.browser.close();
      await owner.browser.close();
    }
  });

  /**
   * A meeting WITHOUT a password must behave exactly as it did before issue
   * 1613 — the control for the two tests above. Without this, "the prompt
   * appears" and "the prompt appears when it should not" look the same.
   */
  test("a meeting with no password never raises the prompt", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_pw_none_${Date.now()}`;

    // `launchAuthenticatedBrowser` already applies BROWSER_ARGS; the 4th
    // parameter is for EXTRA args, and this suite needs none.
    const owner = await launchAuthenticatedBrowser(OWNER_EMAIL, OWNER_NAME, uiURL);
    const attendee = await launchAuthenticatedBrowser(ATTENDEE_EMAIL, ATTENDEE_NAME, uiURL);

    try {
      await createMeeting(OWNER_EMAIL, OWNER_NAME, {
        meetingId,
        waitingRoomEnabled: false,
        // No `password` — the key difference from the test above.
      });

      const ownerPage = owner.page;
      await enterMeetingAsHost(ownerPage, meetingId);

      const attendeePage = attendee.page;
      await fillAndSubmitJoinForm(attendeePage, meetingId, ATTENDEE_NAME);
      const attendeeResult = await joinMeetingFromPage(attendeePage);
      expect(attendeeResult).toBe("in-meeting");

      await expect(attendeePage.getByTestId("meeting-password-prompt")).toHaveCount(0);
    } finally {
      await attendee.browser.close();
      await owner.browser.close();
    }
  });
});
