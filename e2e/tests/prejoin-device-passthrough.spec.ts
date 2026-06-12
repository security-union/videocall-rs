import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for issue #1295 — lobby device selection (camera + speaker) must
 * carry into the meeting.
 *
 * The #1295 fix (UNCOMMITTED at the time these tests were written) has two parts:
 *
 *   1. CAMERA — `videocall-client/src/encode/camera_encoder.rs` +
 *      `dioxus-ui/src/components/host.rs`. A concurrency race let a stale encode
 *      loop bind the WRONG (often default/previous) camera to the shared
 *      `<video id="webcam">` element, especially on the OFF -> select-new -> ON
 *      sequence (the loop captured its deviceId at spawn and could never
 *      retarget, so a superseded loop's late `getUserMedia` + `set_src_object`
 *      clobbered the new loop's device). The fix adds a single-loop /
 *      bound-device / epoch guard so at most one acquire is in flight for the
 *      selected device and a superseded loop self-terminates before binding.
 *
 *   2. SPEAKER — `videocall-client/src/audio/shared_audio_context.rs`. A pre-join
 *      speaker (sinkId) selection was dropped because the shared `AudioContext`
 *      is created lazily AFTER the lobby (the first remote audio decoder builds
 *      it with sink `None`). The fix stashes the desired sink in a thread_local
 *      and re-applies it the instant the context is created.
 *
 * ─── What is deterministic in this stack vs. what is a proxy ────────────────
 *
 * CAMERA (deterministic): the in-meeting local camera renders into
 *   `<video class="self-camera" id="webcam">` (host.rs:1021, `VIDEO_ELEMENT_ID`),
 *   and the camera encoder binds the captured `MediaStream` to it via
 *   `set_src_object` (camera_encoder.rs:2188). The encoder constrains
 *   getUserMedia with `deviceId.exact = <selected id>` (camera_encoder.rs:2094),
 *   so the published track's `getSettings().deviceId` is the ground truth of
 *   "which camera was actually captured", and it is reachable from the page:
 *       document.getElementById("webcam").srcObject
 *           .getVideoTracks()[0].getSettings().deviceId
 *   This is the EXACT assertion #1295 specifies. It distinguishes the
 *   intended camera from a stale/wrong one.
 *
 *   Multiple fake cameras are REQUIRED to make this meaningful. The repo's
 *   `playwright.config.ts` launches plain `--use-fake-device-for-media-stream`,
 *   which (verified empirically) exposes only ONE fake camera (`fake_device_0`),
 *   so "select a non-default camera" is impossible there and a wrong-device bug
 *   would be indistinguishable from correct. This spec therefore overrides
 *   launchOptions (see `test.use` below) to add `device-count=3`, which exposes
 *   three DISTINCT camera deviceIds. We assert the captured deviceId equals the
 *   one we selected AND is not the first ("default-ish") entry.
 *
 * SPEAKER (PROXY only — see the GAP note on the speaker test): the in-call audio
 *   sink lives on the shared Rust-side `AudioContext`, which is NOT exposed to
 *   the page (no global handle, and the sinkId of an AudioContext built in wasm
 *   cannot be read back through any DOM element). So the live in-call sinkId is
 *   NOT assertable from Playwright. We assert the strongest available proxies:
 *   the lobby selection persists to localStorage (`vc_prejoin_speaker_id`) and
 *   that persisted id is restored/applied on the join path (host.rs:410-419
 *   calls `update_speaker_device` with exactly this id) — visible as the
 *   in-meeting speaker control reflecting the chosen device. This proves the
 *   selection survives into the meeting plumbing; it does NOT prove the OS routed
 *   audio to that device. See the per-test GAP comment.
 *
 * ─── In-meeting device picker is a CUSTOM listbox, not a native <select> ─────
 * The in-call device settings modal (device_settings_modal.rs) renders
 * `SettingsGlassSelect`: a `<button id="modal-video-select"|"modal-speaker-select"
 * class="glass-select-trigger">` whose `<span class="glass-select-label">` shows
 * the current device label, and (when open) a `div.glass-select-menu[role=listbox]`
 * of `div.glass-select-option[role=option]` items keyed by LABEL text (the
 * deviceId is not in the DOM). So we operate it by label, mapping deviceId->label
 * from the lobby's native `<select>` (which DOES carry both). Helpers below.
 *
 * Lobby selectors mirror the constants in
 * `dioxus-ui/src/components/pre_join_settings_card.rs`.
 */

// ── Lobby selector constants (mirror pre_join_settings_card.rs) ─────────────
const PREVIEW = '[data-testid="prejoin-preview"]';
const CAMERA_TOGGLE = '[data-testid="prejoin-camera-toggle"]';
const CAMERA_SELECT = '[data-testid="prejoin-camera-select"]';
const SPEAKER_SELECT = '[data-testid="prejoin-speaker-select"]';
const PERMISSION_PROMPT = '[data-testid="prejoin-permission-prompt"]';
const PERMISSION_ALLOW = '[data-testid="prejoin-permission-allow"]';
const SPEAKER_UNSUPPORTED_NOTE = '[data-testid="prejoin-speaker-unsupported-note"]';

// In-meeting markup (host.rs / device_settings_modal.rs).
const SELF_VIDEO = "#webcam"; // VIDEO_ELEMENT_ID — local published camera <video>.
const DEVICE_SETTINGS_BUTTON = ".device-settings-menu-button";
const TAB_VIDEO = "#settings-tab-video";
const MODAL_VIDEO_TRIGGER = "#modal-video-select"; // glass-select trigger button.
const MODAL_SPEAKER_TRIGGER = "#modal-speaker-select"; // glass-select trigger button.
// MicButton is the first primary control, CameraButton is the second (#959).
const CAMERA_CONTROL = ".video-controls-container .video-control-button";

// localStorage keys (mirror context.rs:602-606).
const LS_CAMERA_ID = "vc_prejoin_camera_id";
const LS_SPEAKER_ID = "vc_prejoin_speaker_id";

// In-meeting confirmation marker (attendants.rs:4032).
const MEETING_READY = "Your meeting is ready!";

/**
 * Add a third fake camera to the Chromium launch args for THIS spec only.
 *
 * Playwright's `test.use({ launchOptions })` REPLACES the project's
 * `launchOptions` wholesale (it does not deep-merge `args`), so we must restate
 * the full arg set from `playwright.config.ts` and swap the plain
 * `--use-fake-device-for-media-stream` for the `device-count=3` form. Keeping
 * `--origin-to-force-quic-on` + `--ignore-certificate-errors` preserves the
 * WebTransport-capable launch the default suite uses, so the in-meeting flow
 * (which uses the default transport) behaves identically to other specs.
 *
 * `device-count=3` yields three distinct fake camera deviceIds
 * (`fake_device_0/1/2`) AND three fake mics/speakers — verified empirically
 * against this Chromium. Without this, the headline camera assertion would be
 * meaningless (one camera == no non-default to select).
 */
test.use({
  launchOptions: {
    args: [
      "--ignore-certificate-errors",
      "--origin-to-force-quic-on=127.0.0.1:4433",
      "--use-fake-device-for-media-stream=device-count=3",
      "--use-fake-ui-for-media-stream",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--renderer-process-limit=1",
    ],
  },
});

/**
 * Navigate to a fresh meeting and wait for the pre-join card. A brand-new
 * meeting id makes the current user the owner ("Start Meeting"); we match both
 * labels to stay flow-agnostic.
 */
async function gotoPreJoin(page: Page, meetingId: string) {
  await page.goto(`/meeting/${meetingId}`);
  const actionButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await actionButton.waitFor({ timeout: 30_000 });
  await expect(page.locator(PREVIEW)).toBeVisible({ timeout: 30_000 });
  return actionButton;
}

/**
 * Grant media access from the pre-join prompt. With
 * `--use-fake-ui-for-media-stream` the click auto-grants getUserMedia, the
 * prompt disappears, and the device selects populate.
 */
async function grantMediaAccess(page: Page) {
  const allow = page.locator(PERMISSION_ALLOW);
  await expect(allow).toBeVisible();
  await allow.click();
  await expect(page.locator(PERMISSION_PROMPT)).toBeHidden({ timeout: 15_000 });
  await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });
}

interface DeviceOption {
  value: string; // deviceId
  label: string; // display label (what the in-meeting glass-select keys on)
}

/**
 * Distinct (deviceId, label) options of a lobby native `<select>`, in DOM order.
 * The lobby select is the only place that carries BOTH the deviceId (option
 * value) and the human label, so we read the deviceId->label map here and reuse
 * the labels to drive the in-meeting custom listbox.
 */
async function lobbyOptions(page: Page, selector: string): Promise<DeviceOption[]> {
  const raw = await page
    .locator(`${selector} option`)
    .evaluateAll((opts) =>
      (opts as HTMLOptionElement[]).map((o) => ({ value: o.value, label: o.textContent ?? "" })),
    );
  const seen = new Set<string>();
  const out: DeviceOption[] = [];
  for (const o of raw) {
    if (o.value.length > 0 && !seen.has(o.value)) {
      seen.add(o.value);
      out.push({ value: o.value, label: o.label.trim() });
    }
  }
  return out;
}

/**
 * Read the deviceId of the in-meeting local published camera track from the
 * `<video id="webcam">` srcObject. Returns `null` if no live video track is
 * bound yet. This is the ground-truth of "which camera was actually captured".
 */
async function capturedCameraDeviceId(page: Page): Promise<string | null> {
  return page.locator(SELF_VIDEO).evaluate((el) => {
    const v = el as HTMLVideoElement;
    const stream = v.srcObject as MediaStream | null;
    if (!stream) return null;
    const track = stream.getVideoTracks().find((t) => t.readyState === "live");
    if (!track) return null;
    const id = track.getSettings().deviceId;
    return typeof id === "string" && id.length > 0 ? id : null;
  });
}

/** Open the in-meeting device settings modal. */
async function openDeviceSettings(page: Page) {
  await page.locator(DEVICE_SETTINGS_BUTTON).click();
  await expect(page.locator("#device-settings-dialog")).toBeVisible({ timeout: 15_000 });
}

/**
 * Pick a device in an in-meeting glass-select identified by its trigger id and
 * the target device's LABEL: open the trigger, click the option whose text is
 * the label. The deviceId is not in the option DOM, so the caller maps id->label
 * via `lobbyOptions`.
 */
async function glassSelectByLabel(page: Page, triggerSelector: string, label: string) {
  const trigger = page.locator(triggerSelector);
  await expect(trigger).toBeVisible({ timeout: 15_000 });
  await trigger.click();
  // The menu renders only while open; options are role=option keyed by text.
  const option = page
    .locator(`.glass-select-menu [role="option"]`)
    .filter({ hasText: new RegExp(`^\\s*${escapeRegExp(label)}\\s*$`) });
  await expect(option.first()).toBeVisible({ timeout: 10_000 });
  await option.first().click();
}

/** The current label shown on an in-meeting glass-select trigger. */
async function glassSelectLabel(page: Page, triggerSelector: string): Promise<string> {
  return (await page.locator(`${triggerSelector} .glass-select-label`).textContent())?.trim() ?? "";
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

test.describe("Pre-join device passthrough (#1295)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "PreJoinPassthroughUser");
    });
  });

  // ──────────────────────────────────────────────────────────────────────────
  // 1. CAMERA — lobby-selected non-default camera carries into the meeting.
  //
  // DETERMINISTIC. Pins the user-facing #1295 camera behavior: the camera chosen
  // in the lobby is the one captured/published in the call.
  //
  // Fails on revert IF the carry-over (deviceId -> in-meeting capture) were
  // broken in general. NOTE (honest scope): the host.rs change for #1295 only
  // *removed* a `stop()` that was a no-op on this clean single-loop restore path,
  // so this simple-carry test alone would NOT fail if ONLY the encoder race
  // guard were reverted — the simple path was never racy. It IS load-bearing
  // against any regression that drops/ignores the selected deviceId on join
  // (e.g. binding the default camera instead). The race itself is pinned by
  // test 2 below. We assert against the SECOND camera (index 1), so a
  // "bind first/default camera" bug fails here.
  // ──────────────────────────────────────────────────────────────────────────
  test("lobby-selected camera is the one captured in the meeting @camera", async ({ page }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1295_cam_carry_${Date.now()}`);
    await grantMediaAccess(page);

    // Three distinct cameras are available under device-count=3. Pick a
    // NON-default one (index 1) so a wrong/default bind is detectable.
    const cameras = await lobbyOptions(page, CAMERA_SELECT);
    expect(
      cameras.length,
      "device-count=3 must expose >=2 distinct cameras for this assertion to mean anything",
    ).toBeGreaterThanOrEqual(2);
    const chosenCamera = cameras[1].value;
    expect(chosenCamera).not.toBe(cameras[0].value);

    await page.locator(CAMERA_SELECT).selectOption(chosenCamera);

    // Turn the camera ON in the lobby and confirm the choice persisted.
    const cameraToggle = page.locator(CAMERA_TOGGLE);
    await cameraToggle.click();
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true");
    await expect
      .poll(async () => page.evaluate((k) => localStorage.getItem(k), LS_CAMERA_ID), {
        timeout: 10_000,
      })
      .toBe(chosenCamera);

    // Join the meeting.
    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });

    // GROUND TRUTH: the in-meeting published track must be bound to the camera
    // we selected in the lobby — not the first/default one. Poll because the
    // encoder acquires + binds asynchronously after join.
    await expect
      .poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 })
      .toBe(chosenCamera);

    // Belt-and-suspenders: explicitly assert it is NOT the default-ish first
    // camera, so a regression that silently falls back to camera 0 fails loudly
    // even if some future change made chosenCamera coincide oddly.
    const captured = await capturedCameraDeviceId(page);
    expect(captured, "captured camera must not be the first/default entry").not.toBe(
      cameras[0].value,
    );
  });

  // ──────────────────────────────────────────────────────────────────────────
  // 2. CAMERA RACE — OFF -> select-new-camera -> ON must bind the NEW camera.
  //
  // DETERMINISTIC and ADVERSARIAL. This is the test that pins the #1295 encoder
  // race fix specifically. It reproduces the exact "OFF -> switch -> ON" hole
  // the fix names: select() while DISABLED does not raise `switching`, the live
  // loop captured its deviceId at spawn and can never retarget, so a stale loop
  // could win the `set_src_object` and bind the OLD camera.
  //
  // Sequence (driving the same `start()` guard the fix adds):
  //   a. Lobby-select camera B (an EXPLICIT non-default choice that fires onchange
  //      and persists), camera ON, join. Confirm we are genuinely capturing B.
  //   b. In-meeting toggle camera OFF (CameraButton -> set_enabled(false) + stop()).
  //   c. Open device settings -> Video tab, select a DIFFERENT camera A in the
  //      #modal-video-select glass-select WHILE OFF (on_cam_change ->
  //      camera.select(A) while disabled; no `switching`).
  //   d. Toggle camera ON (CameraButton -> set_enabled(true) + start()).
  //   e. Assert the captured deviceId == A (the new camera), never B.
  //
  // Starting from an explicit B (not the implicit default) means the initial
  // "capturing B" confirmation is real, and the switch crosses to a genuinely
  // different device A — so an "== A" pass at the end cannot be vacuous.
  //
  // On the BUGGY code the stale loop bound to B can clobber #webcam after the new
  // loop binds A, so the captured deviceId settles back on B — the "== A" poll
  // times out (never reaches A) or the post-settle re-read reverts to B, failing
  // the test. On the FIXED code the epoch/canary guard guarantees A. We do the
  // OFF/select/ON in quick succession (no artificial settle waits between them)
  // to keep the two loops genuinely concurrent — an artificial pause would let
  // the stale loop fully drain and mask the race even on buggy code.
  // ──────────────────────────────────────────────────────────────────────────
  test("OFF then switch camera then ON binds the newly selected camera @camera", async ({
    page,
  }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1295_cam_race_${Date.now()}`);
    await grantMediaAccess(page);

    const cameras = await lobbyOptions(page, CAMERA_SELECT);
    expect(
      cameras.length,
      "device-count=3 must expose >=2 distinct cameras to exercise an OFF->switch->ON race",
    ).toBeGreaterThanOrEqual(2);
    // Start from B (index 1, a real non-default selection), switch in-meeting to
    // A (index 0). Crossing two distinct devices is what exercises the race.
    const cameraA = cameras[0];
    const cameraB = cameras[1];

    // (a) Lobby-select B (explicit onchange -> persisted), camera ON, join.
    await page.locator(CAMERA_SELECT).selectOption(cameraB.value);
    await expect
      .poll(async () => page.evaluate((k) => localStorage.getItem(k), LS_CAMERA_ID), {
        timeout: 10_000,
      })
      .toBe(cameraB.value);
    const cameraToggle = page.locator(CAMERA_TOGGLE);
    await cameraToggle.click();
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true");
    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });

    // Confirm we are genuinely capturing camera B before provoking the race —
    // otherwise a later "== A" pass could be vacuous (never crossed devices).
    await expect
      .poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 })
      .toBe(cameraB.value);

    // In-meeting camera control (CameraButton is the 2nd primary control).
    const cameraControl = page.locator(CAMERA_CONTROL).nth(1);
    await expect(cameraControl).toHaveClass(/\bactive\b/, { timeout: 15_000 });

    // (b) Camera OFF.
    await cameraControl.click();
    await expect(cameraControl).toHaveClass(/\boff\b/, { timeout: 15_000 });

    // (c) Switch to camera A via the in-call device settings (Video tab), while
    // the camera is OFF — this is the path that does NOT raise `switching`.
    await openDeviceSettings(page);
    await page.locator(TAB_VIDEO).click();
    await glassSelectByLabel(page, MODAL_VIDEO_TRIGGER, cameraA.label);
    // Confirm the in-call selector now reflects camera A (the select took).
    await expect
      .poll(async () => glassSelectLabel(page, MODAL_VIDEO_TRIGGER), { timeout: 10_000 })
      .toBe(cameraA.label);
    // Close the modal via the explicit close button (more deterministic than
    // relying on Escape focus), then immediately re-enable — keeping the OFF->
    // select->ON window tight so both loops can race.
    await page.locator(".settings-modal-close").click();
    await expect(page.locator("#device-settings-dialog")).toBeHidden({ timeout: 10_000 });

    // (d) Camera ON.
    await expect(cameraControl).toBeVisible({ timeout: 15_000 });
    await cameraControl.click();
    await expect(cameraControl).toHaveClass(/\bactive\b/, { timeout: 15_000 });

    // (e) GROUND TRUTH: the captured camera must be A, never B. On buggy code a
    // superseded loop bound to B can clobber #webcam, so the captured id would
    // settle on B and this poll fails.
    await expect
      .poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 })
      .toBe(cameraA.value);

    // Stability guard against a late stale-loop clobber: the wrong-device race
    // is a LATE bind (the stale loop's getUserMedia resolves after the new
    // loop's). Re-read after a short settle and assert it is STILL A and never
    // reverted to B. On buggy code the late clobber would flip this to B.
    await page.waitForTimeout(2_000);
    const settled = await capturedCameraDeviceId(page);
    expect(
      settled,
      "captured camera must remain the newly selected one (no stale-loop clobber)",
    ).toBe(cameraA.value);
    expect(settled, "captured camera must never revert to the pre-switch camera").not.toBe(
      cameraB.value,
    );
  });

  // ──────────────────────────────────────────────────────────────────────────
  // 3. SPEAKER — lobby-selected speaker carries into the meeting.
  //
  // PROXY (see GAP). The in-call audio sink lives on the wasm-side shared
  // `AudioContext`, which is not exposed to the page, so the LIVE in-call sinkId
  // cannot be read from Playwright. We assert the strongest reachable proxies:
  //   - the lobby speaker selection persists to localStorage (vc_prejoin_speaker_id);
  //   - that persisted id is the one the join path applies via
  //     update_speaker_device (host.rs:410-419) — shown by the in-meeting speaker
  //     control reflecting the chosen device's label.
  //
  // GAP: this does NOT assert the OS actually routed audio to the chosen device,
  // nor that `AudioContext.setSinkId` was called with it. The #1295 speaker fix
  // (re-applying DESIRED_SINK_ID when the lazy AudioContext is created) is
  // therefore only partially covered here. A faithful live-sinkId assertion would
  // need the app to expose the shared AudioContext (or its current sinkId) to the
  // page, OR a 2nd audio peer + an output-capture probe — neither exists in this
  // harness. Reported as a coverage gap.
  //
  // Fails on revert IF speaker persistence/restore were broken. It would NOT
  // fail if only the AudioContext re-apply were reverted (that path is invisible
  // to the page) — hence the GAP. We still gate on the supported (Chromium
  // setSinkId) path so the test is skipped cleanly where speaker selection is
  // unsupported.
  // ──────────────────────────────────────────────────────────────────────────
  test("lobby-selected speaker persists and is restored into the meeting @speaker", async ({
    page,
  }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1295_spk_${Date.now()}`);
    await grantMediaAccess(page);

    const speakerSelect = page.locator(SPEAKER_SELECT);
    await expect(speakerSelect).toBeVisible();

    // Speaker selection is only meaningful where setSinkId is supported. Under
    // Chromium it is (verified: setSinkId present on HTMLMediaElement AND
    // AudioContext). If a future browser/env disables it, the dropdown is
    // read-only and shows the unsupported note — skip rather than assert a
    // selection the UI does not allow.
    if (await speakerSelect.isDisabled()) {
      await expect(page.locator(SPEAKER_UNSUPPORTED_NOTE)).toBeVisible();
      test.skip(true, "setSinkId unsupported in this browser; speaker selection is read-only");
      return;
    }

    const speakers = await lobbyOptions(page, SPEAKER_SELECT);
    expect(
      speakers.length,
      "device-count=3 must expose >=2 distinct speakers to select a non-default one",
    ).toBeGreaterThanOrEqual(2);
    const chosenSpeaker = speakers[1];
    expect(chosenSpeaker.value).not.toBe(speakers[0].value);

    await speakerSelect.selectOption(chosenSpeaker.value);

    // The lobby selection persists (the value the join path will re-apply via
    // update_speaker_device).
    await expect
      .poll(async () => page.evaluate((k) => localStorage.getItem(k), LS_SPEAKER_ID), {
        timeout: 10_000,
      })
      .toBe(chosenSpeaker.value);

    // Join the meeting.
    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });

    // PROXY 1: the persisted speaker preference survives into the meeting (the
    // join path read it and did NOT clear/overwrite it with the default).
    const persistedInMeeting = await page.evaluate((k) => localStorage.getItem(k), LS_SPEAKER_ID);
    expect(persistedInMeeting, "lobby speaker selection must survive into the meeting").toBe(
      chosenSpeaker.value,
    );

    // PROXY 2: open the in-call device settings (Audio tab is default) and assert
    // the speaker control reflects the lobby selection's label (the same id flows
    // into the in-meeting selector via selected_speaker_id). This shows the
    // selection carried into the in-meeting device state, not just localStorage.
    await openDeviceSettings(page);
    await expect
      .poll(async () => glassSelectLabel(page, MODAL_SPEAKER_TRIGGER), { timeout: 15_000 })
      .toBe(chosenSpeaker.label);
  });

  // ──────────────────────────────────────────────────────────────────────────
  // 4. DEFAULT-DEVICE REGRESSION GUARD — join WITHOUT changing devices.
  //
  // DETERMINISTIC. Ensures the #1295 single-loop/epoch guard did NOT break the
  // no-selection happy path: a user who never touches the selectors still gets a
  // working default camera captured in the meeting. The new guard's early
  // not-enabled return and "same device == no respawn" branch must not stall the
  // first cold-start loop.
  //
  // Fails on revert IF the guard regressed the no-selection cold start (e.g. the
  // early-return swallowed the first legitimate loop, leaving #webcam with no
  // live track). It asserts a live captured track with a real deviceId rather
  // than a specific id (the default id is environment-dependent), which is the
  // right invariant for "default works".
  // ──────────────────────────────────────────────────────────────────────────
  test("default camera works when no device is changed in the lobby @camera", async ({ page }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1295_default_${Date.now()}`);
    await grantMediaAccess(page);

    // Do NOT touch the camera/speaker selectors. Just turn the camera ON with
    // whatever device is selected by default, then join.
    const cameraToggle = page.locator(CAMERA_TOGGLE);
    await cameraToggle.click();
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true");

    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });

    // A live default camera track must be captured and bound to #webcam.
    await expect.poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 }).not.toBeNull();

    // It must be a non-empty (real) deviceId, not garbage.
    const captured = await capturedCameraDeviceId(page);
    expect(captured, "default capture must yield a non-empty deviceId").toBeTruthy();

    // The in-meeting camera control reflects the enabled/publishing state.
    const cameraControl = page.locator(CAMERA_CONTROL).nth(1);
    await expect(cameraControl).toHaveClass(/\bactive\b/, { timeout: 15_000 });
    await expect(cameraControl).not.toHaveClass(/\boff\b/);
  });

  // ──────────────────────────────────────────────────────────────────────────
  // 5. JOIN-WITH-CAMERA-ON CONVERGENCE under a forced post-permission
  //    `devicechange` DOUBLE-START.  @camera
  //
  // ─── WHAT THIS PINS (and, crucially, what it does NOT) ──────────────────────
  // The #1295 working-tree fix (camera_encoder.rs) targets a "dark square on
  // initial join": joining with the camera ON showed `<video id="webcam">` with
  // NO live feed until a manual OFF→ON. Root cause: a post-permission
  // `devicechange` fires a SECOND camera start() whose `select()`-while-enabled
  // raises the `switching` flag; on the BUGGY encoder the acquire-phase and
  // per-frame supersede guards READ `switching` and aborted the loop that should
  // bind (acquire bail BEFORE `set_src_object`, or per-frame `track.stop()` on a
  // just-bound track), so no live track survived. The fix makes `epoch` (not
  // `switching`) the sole supersede authority and clears `switching` at the
  // loop-commit, so exactly one loop binds and stays live.
  //
  // THIS TEST IS A CONVERGENCE / "permanent-dark" GUARD, *NOT* a regression
  // guard that fails on the pre-fix encoder. It is deliberately NOT claimed to
  // pin the fix, because — verified against the buggy source at HEAD
  // (75cb1a88, `git show HEAD:videocall-client/src/encode/camera_encoder.rs`) —
  // the buggy dark square is *self-healing in this harness* and cannot be made to
  // fail deterministically with JS injection alone:
  //
  //   • The post-permission `devicechange` handler (host.rs on_devices_changed,
  //     L505-549) both raises `switching` (via `select()` at L521) AND schedules
  //     a healing `camera.start()` 1000ms later (L544). On the buggy encoder the
  //     stale loop self-aborts (acquire bail HEAD L2152-2180 / per-frame stop
  //     HEAD L2565-2571) but ALWAYS clears `switching` on the way out
  //     (HEAD L2172 / L2569). ~1s later the scheduled start() spawns a FRESH loop
  //     into a clean `switching==false` / new-epoch state, so it binds a LIVE
  //     track and HEALS. With fake devices `getUserMedia` resolves in ms, so the
  //     dark interval is sub-second and the end-state is LIVE on buggy too.
  //   • The only JS lever that could park the heal — delaying `getUserMedia` —
  //     ALSO darkens the FIXED build: on a same-device `devicechange` the
  //     start() running-guard takes `self.stop()` + respawn on BOTH builds
  //     (camera_encoder.rs L1911-1925, identical buggy/fixed: `switch_requested`
  //     is true so the same-device early-return is skipped), so the fixed loop is
  //     also torn down and must re-`getUserMedia` to rebind. A parked
  //     `getUserMedia` therefore leaves BOTH builds dark in the same window —
  //     the delay cannot separate them. The faithful, deterministic dark-square
  //     regression guard therefore lives in Rust, not here: the native unit test
  //     `camera_encoder::tests::loop_is_not_superseded_when_only_switching_is_raised`
  //     asserts the extracted supersede predicate (`loop_is_superseded`, the SAME
  //     one both encode-loop guards call) does NOT mark a loop superseded when
  //     only `switching` is raised (enabled, matching epoch) — i.e. the loop that
  //     should bind keeps going instead of bailing before `set_src_object`. That
  //     test FAILS if the pre-fix `switching` term is reintroduced into the
  //     predicate (verified by mutation), so it pins the dark-square fix
  //     deterministically and headlessly. The wrong-device facet of #1295 is
  //     pinned deterministically by test 2 above.
  //
  // ─── WHAT THIS TEST DOES CATCH (its load-bearing invariant) ─────────────────
  // It FORCES the app's real double-start path that test 4 cannot reach. Test 4
  // only ever exercises the SINGLE-start cold path because fake-device
  // `enumerateDevices()` returns an IDENTICAL id list on every call, so the app's
  // `MediaDeviceList` list-diff (media_device_list.rs L377-404) never fires
  // `on_devices_changed`, so no `select()`-while-enabled → `switching` → second
  // start() ever happens. Here we install a passthrough `enumerateDevices` stub
  // that, ONLY after we arm it post-join, returns the REAL device list with one
  // NON-selected video input dropped — a genuinely different id list whose
  // SELECTED camera still exists. Dispatching `devicechange` then drives the app
  // through `on_devices_changed` → same-device `select()` (raises `switching`) →
  // scheduled second `camera.start()` — the exact interleaving the dark square
  // lived in. The invariant asserted is the user-facing contract: after that
  // double-start, `#webcam` CONVERGES to a live bound track (a real, non-empty
  // deviceId). This fails if a future regression makes the double-start path
  // leave the camera PERMANENTLY dark (e.g. the heal never rebinds, or
  // on_devices_changed stops rebinding), which is a real and distinct robustness
  // property. It is NOT vacuous (`X==X`): mutate the heal so the second start()
  // never rebinds and this poll times out null.
  //
  // ─── ADVERSARIAL SELF-CHECK (per CLAUDE.md, stated honestly) ────────────────
  // Check 2 — "does this fail on the buggy encoder?": NO, and that is reported,
  // not hidden. The buggy encoder self-heals within ~1s (walked above), so this
  // test PASSES on buggy HEAD. It is therefore explicitly a convergence /
  // permanent-dark guard, NOT the dark-square fix-pin, and makes no claim to be
  // one. The dark-square fix IS pinned deterministically — by the Rust unit test
  // named above (`loop_is_not_superseded_when_only_switching_is_raised`), which
  // fails on the pre-fix predicate — so the suite satisfies Check 2 for #1295
  // without relying on this e2e test to do it. This test's distinct, load-bearing
  // job is the one the Rust unit cannot reach: driving the app's REAL
  // double-start path end-to-end and proving it CONVERGES (never goes permanently
  // dark). No-shared-meeting-id: this test uses its own Date.now()-suffixed id, so
  // it cannot make test 2 or test 4 flaky.
  // ──────────────────────────────────────────────────────────────────────────
  test("join with camera ON converges to a live track after a forced devicechange double-start @camera", async ({
    page,
  }) => {
    // Arm a passthrough enumerateDevices stub BEFORE the app boots. While
    // `window.__vcDropVideoId` is unset (the whole lobby phase) it is a pure
    // passthrough — the lobby preview, permission grant, and select population
    // behave exactly as in every other test. Only after we set an explicit
    // drop-id post-join does a subsequent enumerate return the REAL list minus
    // that one video input, producing a different id list (so the app's
    // list-diff fires on_devices_changed). We set the drop-id to a video input
    // that is NOT the live-captured camera, so the SELECTED camera always
    // survives → a genuine SAME-device restart (the production dark-square
    // trigger), built from real MediaDeviceInfo objects (no synthesized shapes).
    await page.addInitScript(() => {
      const md = navigator.mediaDevices;
      const realEnumerate = md.enumerateDevices.bind(md);
      md.enumerateDevices = async () => {
        const devices = await realEnumerate();
        const w = window as unknown as { __vcDropVideoId?: string };
        if (typeof w.__vcDropVideoId !== "string") return devices;
        return devices.filter((d) => d.deviceId !== w.__vcDropVideoId);
      };
    });

    const actionButton = await gotoPreJoin(page, `e2e_1295_join_converge_${Date.now()}`);
    await grantMediaAccess(page);

    // Mirror test 4's join: default devices, camera ON, no selector changes.
    const cameraToggle = page.locator(CAMERA_TOGGLE);
    await cameraToggle.click();
    await expect(cameraToggle).toHaveAttribute("aria-pressed", "true");

    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });

    // Baseline: the cold-start loop binds a live track first (single-start path).
    await expect.poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 }).not.toBeNull();
    const baselineCameraId = await capturedCameraDeviceId(page);
    expect(baselineCameraId, "baseline join must capture a live camera").toBeTruthy();

    // Arm the stub to drop a video input that is NOT the live-captured camera on
    // the NEXT enumerate (so the changed list still contains the selected camera
    // → SAME-device restart), then dispatch devicechange so the app
    // re-enumerates, sees a changed id list, and runs on_devices_changed →
    // same-device select() (raises `switching`) → scheduled second
    // camera.start(). This is the forced DOUBLE-START. If there is no other video
    // input to drop, the list cannot be made to differ and the forcing function
    // is a no-op — fail loudly rather than assert a vacuous convergence.
    const dropId = await page.evaluate(async (selectedId) => {
      const devices = await navigator.mediaDevices.enumerateDevices();
      const other = devices.find((d) => d.kind === "videoinput" && d.deviceId !== selectedId);
      if (!other) return null;
      (window as unknown as { __vcDropVideoId?: string }).__vcDropVideoId = other.deviceId;
      navigator.mediaDevices.dispatchEvent(new Event("devicechange"));
      return other.deviceId;
    }, baselineCameraId);
    expect(
      dropId,
      "device-count=3 must expose a second video input to drop so on_devices_changed can fire",
    ).toBeTruthy();

    // CONVERGENCE INVARIANT: after the forced double-start the camera must end up
    // with a live bound track and a real deviceId. On the fixed encoder the bind
    // is held throughout; on the buggy encoder it self-heals within ~1s (see the
    // header) — both converge live, which is why this is a convergence guard, not
    // a fix-pinning guard. The window covers the on_devices_changed Timeout(1000)
    // + a fast getUserMedia rebind with generous margin.
    await expect.poll(async () => capturedCameraDeviceId(page), { timeout: 30_000 }).not.toBeNull();
    const captured = await capturedCameraDeviceId(page);
    expect(
      captured,
      "camera must converge to a non-empty deviceId after the double-start",
    ).toBeTruthy();

    // Settle guard: re-read after a short delay to ensure the converged live
    // track is not transient (no late loop clobbers it back to dark).
    await page.waitForTimeout(2_000);
    const settled = await capturedCameraDeviceId(page);
    expect(
      settled,
      "converged live track must remain bound (no late clobber to dark)",
    ).toBeTruthy();

    // The in-meeting camera control still reflects the enabled/publishing state.
    const cameraControl = page.locator(CAMERA_CONTROL).nth(1);
    await expect(cameraControl).toHaveClass(/\bactive\b/, { timeout: 15_000 });
    await expect(cameraControl).not.toHaveClass(/\boff\b/);
  });
});
