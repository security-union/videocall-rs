import { Page, expect } from "@playwright/test";

/**
 * Robustly fill the home-page "Start or Join a Meeting" form and submit it,
 * waiting until the SPA has actually left the home page and entered the meeting
 * route (`/meeting/<id>`).
 *
 * WHY THIS EXISTS (bvt flake fix):
 * The previous join pattern used a FIXED `page.waitForTimeout(1500)` after
 * `goto("/")` as a stand-in for "WASM hydrated", then pressed Enter and
 * asserted `toHaveURL(/\/meeting\/<id>/, { timeout: 10_000 })`. Under CI's
 * 2-browser + fake-camera contention this was flaky two ways, BOTH reproduced
 * locally against the docker e2e stack (multiple specs, workers:2):
 *
 *   1. SUBMIT-TIMING: the production join form (dioxus-ui/src/pages/home.rs)
 *      only renders its `button[type=submit]` ("Start or Join Meeting") once the
 *      reactive `oninput` handler has written the typed meeting id into the
 *      `meeting_id_value` signal — i.e. once the WASM form is interactive. The
 *      fixed wait did not gate on that, so submission could race the form
 *      becoming live.
 *
 *   2. HISTORY-URL LAG (the dominant failure observed): under heavy main-thread
 *      load the Dioxus router renders the meeting view (the page visibly leaves
 *      home and shows the in-meeting toolbar / "Your meeting is ready!" lobby),
 *      but `window.location` — which `toHaveURL` reads — still reports
 *      `http://localhost:3001/` for several seconds. The test then fails with
 *      "expected /meeting/<id>, got /" even though the app DID navigate. This was
 *      the captured root cause: a stuck-at-`/` page whose DOM was the meeting
 *      view. (Confirmed via an error-context snapshot: url=`/`, body=meeting.)
 *
 * The robust signal for "we navigated into the meeting" is therefore a concrete
 * DOM transition, NOT the lagging URL: the home form's `#meeting-id` input is
 * unique to the home page (home.rs) and is gone once the meeting route mounts.
 * We wait for that, then settle the URL on a best-effort basis (it catches up).
 *
 * This is test-only — it does not require any production change. (A production
 * hydration/route-ready marker would be a nicer durable signal, but that needs
 * a frontend change and is not required to de-flake the join step.)
 */
export async function fillAndSubmitJoinForm(
  page: Page,
  meetingId: string,
  username: string,
  opts: { navTimeoutMs?: number } = {},
): Promise<void> {
  const navTimeoutMs = opts.navTimeoutMs ?? 20_000;
  const meetingUrlRe = new RegExp(`/meeting/${meetingId}`);

  await page.goto("/");

  // The form fields are rendered by the WASM app; wait for the meeting-id
  // input to be visible before interacting (replaces the blind waitForTimeout).
  const meetingIdInput = page.locator("#meeting-id");
  const usernameInput = page.locator("#username");
  await meetingIdInput.waitFor({ state: "visible", timeout: navTimeoutMs });

  // Type the meeting id. The submit button only renders once the production
  // `oninput` handler (post-hydration) writes meeting_id_value -- so the button
  // becoming visible is our "form is interactive" gate.
  await meetingIdInput.click();
  await meetingIdInput.fill("");
  await meetingIdInput.pressSequentially(meetingId, { delay: 30 });

  // Display name is a controlled input -- clear before typing to handle any
  // pre-fill from a previously-saved display name.
  await usernameInput.click();
  await usernameInput.fill("");
  await usernameInput.pressSequentially(username, { delay: 30 });

  // Interactive-form gate: the "Start or Join Meeting" submit button only
  // exists when meeting_id_value is non-empty, which only happens once the
  // reactive oninput handler fired -- i.e. WASM is hydrated and onsubmit is
  // attached. Submitting before this point is the submit-timing race above.
  const submitButton = page.getByRole("button", { name: "Start or Join Meeting" });
  await submitButton.waitFor({ state: "visible", timeout: navTimeoutMs });

  // Submit + retry loop. Click the (now-live) submit button and wait for the
  // home form to DETACH -- the route-changed signal that is robust to the
  // History-API URL lag. `#meeting-id` is unique to the home page, so its
  // disappearance unambiguously means we left "/" for the meeting route. Retry
  // (bounded by navTimeoutMs) only guards against a click landing during a
  // transient re-render.
  const deadline = Date.now() + navTimeoutMs;
  let left = false;
  while (Date.now() < deadline && !left) {
    // If the home form is already gone we've navigated -- stop submitting; the
    // toHaveCount(0) assertion below confirms it authoritatively.
    if ((await meetingIdInput.count()) === 0) {
      break;
    }

    // Re-assert the meeting id in case a re-render cleared it (which would
    // also remove the submit button), then submit.
    const currentMeetingId = await meetingIdInput.inputValue().catch(() => "");
    if (currentMeetingId !== meetingId) {
      await meetingIdInput.click().catch(() => {});
      await meetingIdInput.fill("").catch(() => {});
      await meetingIdInput.pressSequentially(meetingId, { delay: 30 }).catch(() => {});
    }

    if (await submitButton.isVisible().catch(() => false)) {
      await submitButton.click({ timeout: 2_000 }).catch(() => {});
    }

    // Primary success signal: the home form detaches when the meeting route
    // mounts. Robust to window.location lagging behind the rendered route.
    await meetingIdInput
      .waitFor({ state: "detached", timeout: 3_000 })
      .then(() => {
        left = true;
      })
      .catch(() => {});
  }

  // We left the home page. Assert it explicitly so a genuine no-navigation
  // failure (form never detached) surfaces clearly.
  await expect(meetingIdInput).toHaveCount(0, {
    timeout: Math.max(2_000, deadline - Date.now()),
  });

  // Best-effort URL settle: the History-API URL catches up shortly after the
  // route renders. Don't hard-fail on it -- the DOM transition above is the
  // authoritative "we joined" signal -- but give it a chance so callers that
  // read the URL afterwards see the settled value.
  await page.waitForURL(meetingUrlRe, { timeout: 5_000 }).catch(() => {});
}
