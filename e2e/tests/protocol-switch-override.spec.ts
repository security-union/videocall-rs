import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for HCL issue #1291 — "explicit transport choice overrides a
 * remembered pin" (fix committed at 298147d9).
 *
 * The fix has three user-visible halves:
 *
 *   1. An explicit, NOT-remembered protocol choice (via Settings -> Network
 *      Apply, or the diagnostics transport <select>) clears any stale
 *      localStorage sticky pin of the OTHER protocol, so the just-made choice
 *      wins on the next page load instead of being shadowed by the old pin.
 *   2. The "Remember protocol choice" toggle is shown for BOTH protocols (it
 *      used to be hidden for the default, WebTransport), and switching the
 *      protocol radio resets that toggle OFF so a previous protocol's pin
 *      cannot carry over.
 *   3. (Post-review-blocker) The Remember toggle is IN-MEMORY ONLY: toggling it
 *      writes nothing to storage. "Apply" is the sole storage-commit point, and
 *      Apply now appears whenever the staged protocol OR the staged Remember
 *      flag diverges from its persisted value (so a remember-only change on the
 *      same protocol still shows Apply). Closing the modal without Apply writes
 *      nothing.
 *
 *   Final Apply-visibility contract (device_settings_modal.rs):
 *     Apply is shown iff
 *       pending_protocol != active_protocol  OR  sticky_toggle != load_transport_sticky()
 *
 *   Persistence on Apply delegates to `apply_transport_decision(pref, sticky)`:
 *     - default + not sticky  -> clear all keys
 *     - any value + sticky    -> localStorage pref + sticky=true
 *     - non-default + not sticky -> clear stale localStorage pin, write
 *                                   sessionStorage session value
 *
 * --------------------------------------------------------------------------
 * Why we assert PERSISTED STORAGE + the RESTORED UI SELECTION rather than the
 * live transport:
 *
 * The app reloads the page on Apply / confirm (`window.location.reload()`),
 * and the chosen protocol is re-read from storage by `load_transport_preference`
 * at the next App mount (`main.rs`: `use_signal(load_transport_preference)`).
 * The settings modal's segmented control then renders that restored value
 * (`pending_protocol` initialises from the `transport_preference` prop, which
 * flows from `TransportPreferenceCtx`). Asserting the actual negotiated
 * transport of a *live* WebTransport/WebSocket connection is not feasible here
 * without a full two-party meeting and protocol introspection that the UI does
 * not expose deterministically. The persisted storage keys + the restored
 * segmented-control selection after reload are the deterministic proxy for
 * "the choice took hold", and they are exactly the keys the #1291 fix
 * manipulates — so they distinguish the fixed code from the buggy code.
 *
 * Storage keys (see context.rs):
 *   - localStorage  vc_transport_preference  (the sticky/remembered value)
 *   - localStorage  vc_transport_sticky      ("true" when remembered)
 *   - sessionStorage vc_transport_session    (session-scoped, not remembered)
 * --------------------------------------------------------------------------
 */

// Real selectors mirrored from the source. Do NOT invent these — they are the
// stable `data-testid`s / element ids defined in:
//   dioxus-ui/src/components/device_settings_modal.rs
//   dioxus-ui/src/components/diagnostics.rs
const SEL = {
  meetingId: "#meeting-id",
  username: "#username",
  grid: "#grid-container",
  openSettings: '[data-testid="open-settings"]',
  modal: ".device-settings-modal",
  navNetwork: '[data-testid="settings-nav-network"]',
  navActive: ".settings-nav-button.active",
  radioWebTransport: '[data-testid="transport-radio-webtransport"]',
  radioWebSocket: '[data-testid="transport-radio-websocket"]',
  applyButton: '[data-testid="transport-apply-button"]',
  stickyCheckbox: "#sticky-transport-checkbox",
  diagTransportSelect: "#diagnostics-transport-select",
} as const;

const KEYS = {
  pref: "vc_transport_preference",
  sticky: "vc_transport_sticky",
  session: "vc_transport_session",
} as const;

type TransportStorage = {
  pref: string | null;
  sticky: string | null;
  session: string | null;
};

/**
 * Read all three transport storage keys at once. Used after reload as the
 * deterministic proxy for which protocol "took hold".
 */
async function readTransportStorage(page: Page): Promise<TransportStorage> {
  return page.evaluate(
    (k) => ({
      pref: localStorage.getItem(k.pref),
      sticky: localStorage.getItem(k.sticky),
      session: sessionStorage.getItem(k.session),
    }),
    KEYS,
  );
}

/** Clear every transport key so each test starts from a known-clean slate. */
async function clearTransportStorage(page: Page): Promise<void> {
  await page.evaluate((k) => {
    localStorage.removeItem(k.pref);
    localStorage.removeItem(k.sticky);
    sessionStorage.removeItem(k.session);
  }, KEYS);
}

/**
 * Navigate to a meeting room and join as a single user. Mirrors the helper in
 * protocol-selection.spec.ts so behaviour/timeouts stay consistent.
 */
async function joinMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator(SEL.meetingId).click();
  await page.locator(SEL.meetingId).pressSequentially(meetingId, { delay: 80 });

  await page.locator(SEL.username).click();
  await page.locator(SEL.username).fill("");
  await page.locator(SEL.username).pressSequentially(username, { delay: 80 });
  await page.waitForTimeout(500);
  await page.locator(SEL.username).press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();

  await expect(page.locator(SEL.grid)).toBeVisible({ timeout: 15_000 });
}

/** Open the settings modal via the gear icon in the bottom toolbar. */
async function openSettingsModal(page: Page): Promise<void> {
  await page.locator(SEL.openSettings).click();
  await expect(page.locator(SEL.modal)).toBeVisible({ timeout: 10_000 });
}

/** Switch to the Network tab inside the settings modal. */
async function switchToNetworkTab(page: Page): Promise<void> {
  await page.locator(SEL.navNetwork).click();
  await expect(page.locator(SEL.navActive)).toContainText("Network");
}

/**
 * Open settings, go to Network, and wait for the segmented control to render.
 */
async function openNetworkTab(page: Page): Promise<void> {
  await openSettingsModal(page);
  await switchToNetworkTab(page);
  await expect(page.locator(SEL.radioWebTransport)).toBeVisible({ timeout: 10_000 });
}

/**
 * Seed a sticky (remembered) localStorage pin for the given protocol BEFORE the
 * app boots, then reload so `load_transport_preference` picks it up on mount.
 * This is the "the OTHER protocol is already pinned" precondition for #1291.
 */
async function seedStickyPinAndReload(page: Page, protocol: "websocket" | "webtransport") {
  await page.goto("/");
  await page.waitForTimeout(1500);
  await page.evaluate(
    ({ k, protocol }) => {
      localStorage.setItem(k.pref, protocol);
      localStorage.setItem(k.sticky, "true");
    },
    { k: KEYS, protocol },
  );
  await page.reload();
}

// ---------------------------------------------------------------------------

test.describe("Transport protocol switch overrides a remembered pin (#1291)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  // -------------------------------------------------------------------------
  // 1. HEADLINE: WebSocket pinned (Remember ON), then switch to WebTransport in
  //    Settings -> Network and Apply. The stale WS sticky pin must be cleared
  //    and the resolved selection must be WebTransport.
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   In the buggy code the "Remember" toggle was hidden for WebTransport AND
  //   the radio click did NOT reset `sticky_transport`, so it stayed `true`
  //   (seeded from the WS pin). Apply therefore ran the `(is_default, sticky) =
  //   (true, true)` arm -> `save_transport_preference(WebTransport)` +
  //   `save_transport_sticky(true)`, leaving BOTH localStorage keys present
  //   (pref="webtransport", sticky="true"). The fix resets the toggle OFF on
  //   the radio switch, so Apply runs the `(true, false)` arm ->
  //   `clear_transport_sticky_and_pref()`, leaving the keys ABSENT. The
  //   `toBeNull()` assertions below pass only with the fix.
  // -------------------------------------------------------------------------
  test("switching to WebTransport (not remembered) clears a stale WebSocket sticky pin", async ({
    page,
  }) => {
    const meetingId = `e2e_1291_ws_to_wt_${Date.now()}`;

    await seedStickyPinAndReload(page, "websocket");
    await joinMeeting(page, meetingId, "ovr-user-1");

    await openNetworkTab(page);

    // Precondition: the seeded WS pin is reflected (WebSocket selected, sticky
    // checkbox checked). This proves the "OTHER protocol is pinned" state.
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    // User explicitly picks WebTransport. The fix resets the Remember toggle to
    // OFF on this radio switch (see assertion in test 3) — here we just commit.
    await page.locator(SEL.radioWebTransport).click();
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");

    await expect(page.locator(SEL.applyButton)).toBeVisible();
    await page.locator(SEL.applyButton).click();

    // Apply reloads the page; wait for it to settle.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    // The stale WS pin must be GONE from localStorage (this is the #1291 fix).
    const storage = await readTransportStorage(page);
    expect(storage.pref).toBeNull();
    expect(storage.sticky).toBeNull();
    // Choosing the default not-remembered clears the session key too.
    expect(storage.session).toBeNull();

    // And the restored UI selection resolves to WebTransport — the deterministic
    // proxy for "WebTransport actually took hold on reload".
    await joinMeeting(page, `${meetingId}_after`, "ovr-user-1b");
    await openNetworkTab(page);
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "false");

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 2. The "Remember protocol choice" toggle is visible for BOTH protocols.
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   The buggy code gated the sticky row behind
  //   `pending_protocol() != TransportPreference::default()`, so the toggle was
  //   absent whenever WebTransport (the default) was selected. The
  //   `toBeVisible()` assertion for the WebTransport case below fails against
  //   that code. The fix renders the row for both protocols.
  // -------------------------------------------------------------------------
  test("Remember toggle is visible for both WebTransport and WebSocket", async ({ page }) => {
    const meetingId = `e2e_1291_toggle_both_${Date.now()}`;
    await joinMeeting(page, meetingId, "ovr-user-2");

    await openNetworkTab(page);

    // WebSocket selected -> toggle visible (true even before the fix).
    await page.locator(SEL.radioWebSocket).click();
    await expect(page.locator(SEL.stickyCheckbox)).toBeVisible();

    // WebTransport (the default) selected -> toggle MUST still be visible. This
    // is the half of #1291 that lets a user clear a stuck pin from the default.
    await page.locator(SEL.radioWebTransport).click();
    await expect(page.locator(SEL.stickyCheckbox)).toBeVisible();

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 3. Switching the protocol radio resets the Remember toggle to OFF (no
  //    carry-over of the previous protocol's pin).
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   The buggy radio onclick was just `pending_protocol.set(value)` — it never
  //   touched `sticky_transport`. With a seeded WS pin, the toggle stays CHECKED
  //   after switching to WebTransport, so `toBeChecked()` would hold (and
  //   `not.toBeChecked()` fails). The fix adds
  //   `if pending_protocol() != value { sticky_transport.set(false) }`.
  // -------------------------------------------------------------------------
  test("switching the protocol radio resets the Remember toggle to OFF", async ({ page }) => {
    const meetingId = `e2e_1291_toggle_reset_${Date.now()}`;

    // Seed a WS sticky pin so the toggle starts CHECKED on the WS selection.
    await seedStickyPinAndReload(page, "websocket");
    await joinMeeting(page, meetingId, "ovr-user-3");

    await openNetworkTab(page);

    // Seeded WS pin: WebSocket selected and the toggle is ON.
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    // Switch to WebTransport — the toggle must reset to OFF (in-memory only;
    // storage is untouched until Apply, which test 1 covers).
    await page.locator(SEL.radioWebTransport).click();
    await expect(page.locator(SEL.stickyCheckbox)).not.toBeChecked();

    // Switching back to WebSocket is also a fresh, uncommitted choice, so the
    // toggle stays OFF (it does NOT re-read the still-present stale pin).
    await page.locator(SEL.radioWebSocket).click();
    await expect(page.locator(SEL.stickyCheckbox)).not.toBeChecked();

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 4. Remember ON for WebTransport persists across a reload — committed via
  //    Apply, NOT eagerly. The Remember checkbox is now in-memory only; Apply
  //    is the sole storage-commit point, and Apply appears for a remember-only
  //    change on the same protocol.
  //
  //    Flow: WebTransport selected (the default, unchanged) -> toggle Remember
  //    ON -> assert localStorage is STILL EMPTY (the toggle wrote nothing) ->
  //    Apply (now visible because the sticky flag differs from its persisted
  //    value) -> after reload the localStorage pin is present and the selection
  //    + checkbox are restored.
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   The pre-blocker code wrote storage eagerly in the checkbox `onchange`
  //   (`save_transport_preference` + `save_transport_sticky`). Against that code
  //   the "localStorage STILL empty after toggling, before Apply" assertion
  //   below fails — the keys would already be `webtransport` / `true`. It also
  //   fails against the original (pre-#1291) code, where the toggle was not even
  //   rendered for WebTransport so `check()` would throw on a hidden element.
  //   This test now pins BOTH the #1291 toggle-visible-for-default fix AND the
  //   blocker fix (no eager write).
  // -------------------------------------------------------------------------
  test("Remember ON for WebTransport is committed via Apply (no eager write) and survives reload", async ({
    page,
  }) => {
    const meetingId = `e2e_1291_wt_remember_${Date.now()}`;
    await joinMeeting(page, meetingId, "ovr-user-4");

    await openNetworkTab(page);

    // WebTransport (the default) is the active selection; no pin is set, so the
    // Remember toggle starts OFF and Apply is hidden (nothing differs yet).
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");
    const sticky = page.locator(SEL.stickyCheckbox);
    await expect(sticky).toBeVisible();
    await expect(sticky).not.toBeChecked();
    await expect(page.locator(SEL.applyButton)).not.toBeVisible();

    // Toggle Remember ON. This is a remember-only change on the SAME protocol.
    await sticky.check({ force: true });

    // The toggle must have written NOTHING to storage yet — this is the blocker
    // fix. Storage stays empty until Apply.
    const beforeApply = await readTransportStorage(page);
    expect(beforeApply.pref).toBeNull();
    expect(beforeApply.sticky).toBeNull();
    expect(beforeApply.session).toBeNull();

    // Apply now appears precisely because the staged sticky flag differs from
    // its persisted value (protocol is unchanged) — the new Apply-visibility
    // contract.
    await expect(page.locator(SEL.applyButton)).toBeVisible();
    await page.locator(SEL.applyButton).click();

    // Apply reloads the page; wait for it to settle.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    // Now (and only now) the localStorage pin is present.
    const afterReload = await readTransportStorage(page);
    expect(afterReload.pref).toBe("webtransport");
    expect(afterReload.sticky).toBe("true");

    // The restored UI reflects WebTransport selected with Remember ON.
    await joinMeeting(page, `${meetingId}_after`, "ovr-user-4b");
    await openNetworkTab(page);
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 5. Diagnostics transport <select>: changing it after a WS pin switches the
  //    protocol AND clears the stale pin (the diagnostics select is an explicit,
  //    NOT-remembered choice — it has no Remember checkbox).
  //
  //    Precondition: WebTransport pinned (Remember ON). Diagnostics select ->
  //    WebSocket. Accept the confirm() dialog (it reloads).
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   The buggy diagnostics onchange passed `load_transport_sticky()` (== true,
  //   because WT is pinned) to `confirm_transport_change`, so it ran the
  //   `(_, true)` arm and wrote localStorage pref="websocket" + sticky="true"
  //   (a NEW sticky pin against the user's intent), and never wrote the session
  //   key. The fix passes `sticky = false`, so it runs the `(false, false)` arm:
  //   clear the stale WT localStorage pin FIRST, then write
  //   sessionStorage vc_transport_session="websocket". The assertions below
  //   (session == "websocket" AND localStorage keys null) hold only with the fix.
  // -------------------------------------------------------------------------
  test("diagnostics transport select makes a not-remembered choice that clears a stale pin", async ({
    page,
  }) => {
    const meetingId = `e2e_1291_diag_switch_${Date.now()}`;

    await seedStickyPinAndReload(page, "webtransport");
    await joinMeeting(page, meetingId, "ovr-user-5");

    // Confirm the protocol-change dialog so the change commits + reloads.
    page.on("dialog", async (dialog) => {
      await dialog.accept();
    });

    const diagSelect = page.locator(SEL.diagTransportSelect);
    await expect(diagSelect).toBeVisible({ timeout: 10_000 });
    // Sanity: seeded WT pin is the current value.
    await expect(diagSelect).toHaveValue("webtransport");

    await diagSelect.selectOption("websocket");

    // Accepting the dialog reloads the page; wait for it to settle.
    await page.waitForLoadState("domcontentloaded", { timeout: 15_000 });
    await page.waitForTimeout(2000);

    const storage = await readTransportStorage(page);
    // Session-scoped WebSocket choice wins...
    expect(storage.session).toBe("websocket");
    // ...and the stale WebTransport localStorage sticky pin is cleared.
    expect(storage.pref).toBeNull();
    expect(storage.sticky).toBeNull();

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 6. Lifecycle guard (the "N1-out" decision): open the modal, switch the
  //    protocol radio, then CLOSE the modal WITHOUT Apply. The previously
  //    confirmed pin must be preserved (storage unchanged) and a reload must
  //    still resolve to the old protocol.
  //
  // WHY THIS MATTERS / WOULD FAIL UNDER A NAIVE "FIX":
  //   The fix deliberately makes the radio click an IN-MEMORY-ONLY reset (it
  //   does NOT write storage), with Apply as the sole commit point. If a future
  //   change moved the storage mutation onto the radio click (the tempting but
  //   wrong approach), switching the radio and then abandoning the modal would
  //   wipe the confirmed WS pin. This test pins that "abandon == no-op" contract:
  //   it asserts the WS pin is byte-for-byte unchanged after closing without
  //   Apply, and that a reload still restores WebSocket.
  // -------------------------------------------------------------------------
  test("abandoning the modal after a radio switch (no Apply) preserves the confirmed pin", async ({
    page,
  }) => {
    const meetingId = `e2e_1291_abandon_${Date.now()}`;

    await seedStickyPinAndReload(page, "websocket");
    await joinMeeting(page, meetingId, "ovr-user-6");

    await openNetworkTab(page);

    // Confirmed pin precondition.
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    const before = await readTransportStorage(page);
    expect(before).toMatchObject({ pref: "websocket", sticky: "true" });

    // Switch the radio to WebTransport — but do NOT click Apply.
    await page.locator(SEL.radioWebTransport).click();
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");

    // Storage must be unchanged by the uncommitted radio click.
    const afterRadio = await readTransportStorage(page);
    expect(afterRadio).toEqual(before);

    // Abandon the modal (Escape closes it without Apply).
    await page.keyboard.press("Escape");
    await expect(page.locator(SEL.modal)).not.toBeVisible({ timeout: 5000 });

    // Storage still unchanged after closing.
    const afterClose = await readTransportStorage(page);
    expect(afterClose).toEqual(before);

    // A reload still resolves to the old (WebSocket) protocol: keys intact and
    // the restored selection is WebSocket.
    await page.reload();
    await page.waitForTimeout(1500);

    const afterReload = await readTransportStorage(page);
    expect(afterReload).toMatchObject({ pref: "websocket", sticky: "true" });

    await joinMeeting(page, `${meetingId}_after`, "ovr-user-6b");
    await openNetworkTab(page);
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    await clearTransportStorage(page);
  });

  // -------------------------------------------------------------------------
  // 7. BLOCKER REGRESSION GUARD: toggling the Remember checkbox (a checkbox
  //    change, NOT a radio change) and then closing the modal WITHOUT Apply
  //    must write NOTHING to storage, and a reload must still resolve to the
  //    prior state.
  //
  //    This is the direct regression guard for the review blocker: the Remember
  //    checkbox used to persist eagerly in its `onchange`
  //    (`save_transport_preference` + `save_transport_sticky`). The blocker fix
  //    made the toggle in-memory only, with Apply as the sole commit point.
  //
  //    Two sub-cases are checked so the guard catches an eager write in EITHER
  //    direction:
  //      (a) From a CLEAN slate (no pin): toggle Remember ON, close without
  //          Apply -> storage must stay empty (an eager write would have set
  //          pref/sticky).
  //      (b) From a SEEDED WS pin: toggle Remember OFF, close without Apply ->
  //          the pin must be byte-for-byte intact (an eager clear-on-uncheck
  //          would have wiped it).
  //
  // WHY THIS FAILS IF THE FIX IS REVERTED:
  //   Against the pre-blocker code, sub-case (a)'s "storage still empty"
  //   assertion fails (the checkbox wrote pref="webtransport"/sticky="true" on
  //   toggle), and sub-case (b)'s "pin intact" assertion fails (the checkbox
  //   ran `clear_transport_sticky_and_pref()` on uncheck). The current
  //   in-memory-only toggle leaves storage untouched until Apply.
  // -------------------------------------------------------------------------
  test("toggling Remember then closing without Apply writes nothing to storage", async ({
    page,
  }) => {
    // ---- Sub-case (a): clean slate, toggle Remember ON, abandon ----
    const meetingIdA = `e2e_1291_toggle_abandon_on_${Date.now()}`;
    await joinMeeting(page, meetingIdA, "ovr-user-7a");

    await openNetworkTab(page);

    // Clean slate: WebTransport selected, Remember OFF, nothing persisted.
    await expect(page.locator(SEL.radioWebTransport)).toHaveAttribute("aria-checked", "true");
    const cleanBefore = await readTransportStorage(page);
    expect(cleanBefore).toEqual({ pref: null, sticky: null, session: null });

    // Toggle Remember ON — a checkbox change, no radio change.
    await page.locator(SEL.stickyCheckbox).check({ force: true });
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    // The toggle must not have written anything yet.
    const cleanAfterToggle = await readTransportStorage(page);
    expect(cleanAfterToggle).toEqual({ pref: null, sticky: null, session: null });

    // Abandon the modal (Escape, no Apply).
    await page.keyboard.press("Escape");
    await expect(page.locator(SEL.modal)).not.toBeVisible({ timeout: 5000 });

    // Storage is still empty after closing.
    const cleanAfterClose = await readTransportStorage(page);
    expect(cleanAfterClose).toEqual({ pref: null, sticky: null, session: null });

    // A reload still resolves to the default WebTransport with no pin.
    await page.reload();
    await page.waitForTimeout(1500);
    const cleanAfterReload = await readTransportStorage(page);
    expect(cleanAfterReload).toEqual({ pref: null, sticky: null, session: null });

    // ---- Sub-case (b): seeded WS pin, toggle Remember OFF, abandon ----
    const meetingIdB = `e2e_1291_toggle_abandon_off_${Date.now()}`;
    await seedStickyPinAndReload(page, "websocket");
    await joinMeeting(page, meetingIdB, "ovr-user-7b");

    await openNetworkTab(page);

    // Seeded WS pin: WebSocket selected, Remember ON.
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();
    const pinnedBefore = await readTransportStorage(page);
    expect(pinnedBefore).toMatchObject({ pref: "websocket", sticky: "true" });

    // Toggle Remember OFF — a checkbox change only; do NOT touch the radio and
    // do NOT click Apply.
    await page.locator(SEL.stickyCheckbox).uncheck({ force: true });
    await expect(page.locator(SEL.stickyCheckbox)).not.toBeChecked();

    // The uncheck must not have cleared storage.
    const pinnedAfterToggle = await readTransportStorage(page);
    expect(pinnedAfterToggle).toEqual(pinnedBefore);

    // Abandon the modal.
    await page.keyboard.press("Escape");
    await expect(page.locator(SEL.modal)).not.toBeVisible({ timeout: 5000 });

    // Pin is byte-for-byte intact after closing.
    const pinnedAfterClose = await readTransportStorage(page);
    expect(pinnedAfterClose).toEqual(pinnedBefore);

    // A reload still resolves to the pinned WebSocket protocol.
    await page.reload();
    await page.waitForTimeout(1500);
    const pinnedAfterReload = await readTransportStorage(page);
    expect(pinnedAfterReload).toMatchObject({ pref: "websocket", sticky: "true" });

    await joinMeeting(page, `${meetingIdB}_after`, "ovr-user-7c");
    await openNetworkTab(page);
    await expect(page.locator(SEL.radioWebSocket)).toHaveAttribute("aria-checked", "true");
    await expect(page.locator(SEL.stickyCheckbox)).toBeChecked();

    await clearTransportStorage(page);
  });
});
