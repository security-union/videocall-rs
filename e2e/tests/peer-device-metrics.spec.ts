import { test, expect, Page, BrowserContext, Browser, chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E: per-peer device / hardware metrics (issue #1482 — "show cpu, memory, and
 * hardware a peer uses if available").
 *
 * A peer self-reports its device facts in its periodic HealthPacket (the ~5 s
 * health interval): OS, device type, CPU cores, silicon architecture, device
 * memory, and main-thread load. The receiver parses those fields
 * (`video_call_client.rs` → `set_peer_device_info`) and surfaces them on TWO
 * render surfaces. Both surfaces render NOTHING when the peer reported nothing
 * (`Option::None` → omitted), so a real remote browser peer that actually emits
 * the metrics is required — a mock/video-off placeholder would never publish a
 * HealthPacket with device fields. This spec therefore drives a genuine 2-peer
 * call (host + camera-on guest) and waits through at least one health interval.
 *
 * The data plane is fully present in the e2e stack: the UI loads
 * `/console-log-collector.js`, which publishes `window.__videocall_client_metadata`
 * (OS / device_type / device_memory_gb / architecture) from `navigator.*` +
 * `userAgentData.getHighEntropyValues`; `navigator.hardwareConcurrency` feeds the
 * Cores field directly. All of OS / Device / Cores / Architecture / Memory are
 * reliably populated under the headless-Chromium runner used by Playwright.
 *
 * Note (issue #1606): the always-`0%` "Main-thread load" segment was REMOVED
 * from both user-facing device lines below. In the browser the value is
 * `sum(longtask_ms) / interval_ms`, a genuine `0.0` while the main thread is
 * idle, so it read `0% load` for nearly everyone; true process/CPU % is not
 * obtainable in browser JS. The field still flows to the proto + health reporter
 * + Prometheus, but no longer appears on the popup line or as a device-row.
 *
 * ## Surface 1 — Signal-quality popup ("Device" line)
 *
 *   div.signal-popup-device  [data-testid="signal-popup-device-{peer_id}"]
 *     span.signal-popup-device__head   "Device"
 *     span.signal-popup-device__line   "macOS 14.5 · desktop · 8 cores · arm · 8 GB"
 *
 * The popup resolves device info per OPEN tile and is NOT gated on the receive
 * list — it renders the moment the peer's HealthPacket device fields have been
 * seen. (`signal_quality.rs::SignalQualityPopup`.)
 *
 * ## Surface 2 — Diagnostics drawer ("Per-peer hardware" sub-block)
 *
 * issue #1606: this sub-block is now a collapsible `<details>` (collapsed by
 * default to save vertical space with many attendees), reusing the drawer's
 * `diag-disclosure` pattern. The block's rows are hidden until the summary is
 * clicked, so this spec EXPANDS the disclosure before reading the rows.
 *
 *   details.diag-disclosure.diag-device
 *     summary.diag-disclosure-summary   "Per-peer hardware (N)"
 *     div.diag-device-peer     [data-testid="diag-device-peer-{session_id}"]
 *       span.diag-device-peer-label   {peer label}
 *       div.diag-device-row
 *         span.diag-device-row-label   {label, e.g. "Cores"}
 *         span.diag-device-row-value   {value, e.g. "8"}
 *
 * This sub-block lives under "Simulcast layers" in the right-side Diagnostics
 * drawer (`#diagnostics-sidebar`), rendered alongside the per-peer RECEIVE list
 * by `SimulcastReceiveBreakdown`. As of the #1482 follow-up it iterates the
 * ALL-PEERS device reader (`reader.per_peer_device_all` → wired to
 * `client.all_peer_device_info()` in `host.rs`) — NOT the receive list — so it
 * renders one block for every known peer that has self-reported device info via
 * its HealthPacket, INDEPENDENT of whether media is currently flowing. A peer
 * that reports device metrics but has its camera OFF (no media on the receive
 * list, no canvas tile) therefore STILL appears here, because
 * `all_peer_device_info` walks the UNION of live peers and the device-info cache
 * (`peer_decode_manager.rs::all_peer_device_info`, fed by `set_peer_device_info`
 * on every HEALTH packet), skipping only peers whose device info is entirely
 * default. The per-peer label still mirrors the receive-list label (display
 * name → user id → session id), so a receiving peer's label is unchanged.
 * (`diagnostics.rs` `device_blocks` / `SimulcastReceiveBreakdown`.)
 *
 * ## Harness lineage
 *
 * The 2-peer camera-on join flow mirrors the proven helpers in
 * `signal-quality-peer-transport.spec.ts` (popup) and `simulcast-per-receiver.spec.ts`
 * (diagnostics drawer): home form → meeting URL → pre-join card → grant media +
 * camera-ON seed (`vc_prejoin_camera_on=true`) → race the Start/Join button vs.
 * the grid, admitting via the host's Waiting Room when the guest is parked.
 *
 * SERIAL + extended timeout: two heavy WebCodecs renderers (publisher encode +
 * receiver decode) plus a full health-interval wait. Matches the serial-mode
 * mitigation used by the simulcast spec for the 8-vCPU CI runner.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/**
 * Drive a context from the home form into the meeting URL, optionally seeding the
 * camera-ON pre-join preference BEFORE navigation so the publisher actually emits
 * video (real browser peers default camera-OFF; with the seed media flows and the
 * publisher's tile decodes a canvas on the host). When `cameraOn` is false the
 * seed is left at its default (camera OFF): no media flows, so the host renders no
 * canvas tile for this peer and the diagnostics RECEIVE list stays empty for it —
 * but its HealthPackets still carry device fields on the ~5 s timer, so the
 * diagnostics "Device (per peer)" sub-block (which iterates ALL known peers via
 * `all_peer_device_info`, not the receive list) STILL renders a block for it.
 * Does NOT click Start/Join yet (that is handled by `clickJoinAndEnterGrid` so the
 * waiting-room admit flow can be interleaved).
 */
async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
  cameraOn = true,
): Promise<Page> {
  const page = await context.newPage();
  if (cameraOn) {
    await page.addInitScript(() => {
      try {
        window.localStorage.setItem("vc_prejoin_camera_on", "true");
      } catch {
        /* storage may be unavailable before origin navigation */
      }
    });
  }

  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  return page;
}

/**
 * Ensure the pre-join camera toggle is ON and a live preview track exists, so
 * the in-meeting encoder starts and the publisher actually sends video. The
 * persisted `vc_prejoin_camera_on=true` seed is the primary lever; this is the
 * belt-and-suspenders click + live-track wait mirroring the simulcast spec.
 */
async function ensurePrejoinCameraOn(page: Page): Promise<void> {
  const allow = page.locator('[data-testid="prejoin-permission-allow"]');
  if (await allow.isVisible().catch(() => false)) {
    await allow.click();
    await page
      .locator('[data-testid="prejoin-permission-prompt"]')
      .waitFor({ state: "hidden", timeout: 15_000 })
      .catch(() => {
        /* already granted / prompt absent */
      });
  }

  const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
  if (!(await cameraToggle.isVisible().catch(() => false))) {
    return;
  }

  if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
    await cameraToggle.click();
  }
  await expect(cameraToggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

  await expect
    .poll(
      async () =>
        page
          .locator('[data-testid="prejoin-camera-preview"]')
          .evaluate((el) => {
            const v = el as HTMLVideoElement;
            const s = v.srcObject as MediaStream | null;
            return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
          })
          .catch(() => 0),
      { timeout: 15_000 },
    )
    .toBeGreaterThan(0);
}

/**
 * Race the pre-join Start/Join button against the grid (some joins auto-advance);
 * when the button appears, optionally turn the camera on and click it. Mirrors
 * `signal-quality-peer-transport.spec.ts::clickJoinAndEnterGrid`. When `cameraOn`
 * is false the pre-join camera is left OFF (default), so this peer publishes no
 * media — the camera-OFF case exercised by the third test.
 */
async function clickJoinAndEnterGrid(page: Page, cameraOn = true): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    if (cameraOn) {
      await ensurePrejoinCameraOn(page);
    }
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Bring a host + one camera-on guest into the same meeting grid, handling the
 * waiting-room admit flow when the guest is parked. Returns the two members with
 * their live pages; the host sees exactly one remote peer tile.
 */
async function standUpTwoPeerCall(
  browsers: Browser[],
  uiURL: string,
  meetingId: string,
): Promise<MeetingMember[]> {
  const profiles = [
    { email: "host-dev@videocall.rs", name: "DevHost" },
    { email: "guest-dev@videocall.rs", name: "DevGuest" },
  ];

  const members: MeetingMember[] = [];
  for (let i = 0; i < 2; i++) {
    const ctx = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    members.push({
      page: null as unknown as Page,
      context: ctx,
      email: profiles[i].email,
      name: profiles[i].name,
    });
  }

  // Host joins first so the meeting is "active" before the guest arrives.
  members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
  await clickJoinAndEnterGrid(members[0].page);

  // Guest joins. Handle direct-join / waiting-room / auto-join.
  members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);

  const joinButton = members[1].page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = members[1].page.getByText("Waiting to be admitted");
  const guestGrid = members[1].page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = members[0].page.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await members[0].page.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await members[0].page.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(members[1].page);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }

  // Host should see exactly one remote peer tile.
  await expect(members[0].page.locator("#grid-container .canvas-container")).toHaveCount(1, {
    timeout: 30_000,
  });

  return members;
}

/**
 * Bring a host (camera ON) + one CAMERA-OFF guest into the same meeting grid,
 * handling the waiting-room admit flow. This is the #1482-follow-up case: the
 * guest publishes NO media, so the host decodes NO `<canvas>` for it (its tile is
 * an avatar — `.canvas-container` without `video-on`, no `<canvas>` child) and the
 * diagnostics RECEIVE list never lists it — but the guest still emits HealthPackets
 * with device fields on the ~5 s timer, so the host registers it via
 * `set_peer_device_info` and `all_peer_device_info` returns it. Presence is gated
 * on the guest's grid TILE (located by display name), which exists for a
 * camera-off peer (an avatar tile, no `<canvas>`) — NOT on a canvas/decode count,
 * which a camera-off peer would never satisfy.
 */
async function standUpHostAndCameraOffGuest(
  browsers: Browser[],
  uiURL: string,
  meetingId: string,
): Promise<MeetingMember[]> {
  const profiles = [
    { email: "host-dev@videocall.rs", name: "DevHost" },
    { email: "guest-dev@videocall.rs", name: "DevGuestOff" },
  ];

  const members: MeetingMember[] = [];
  for (let i = 0; i < 2; i++) {
    const ctx = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    members.push({
      page: null as unknown as Page,
      context: ctx,
      email: profiles[i].email,
      name: profiles[i].name,
    });
  }

  // Host joins first (camera ON) so the meeting is "active" before the guest.
  members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name, true);
  await clickJoinAndEnterGrid(members[0].page, true);

  // Guest joins CAMERA OFF (no vc_prejoin_camera_on seed → default OFF).
  members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name, false);

  const joinButton = members[1].page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = members[1].page.getByText("Waiting to be admitted");
  const guestGrid = members[1].page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = members[0].page.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await members[0].page.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await members[0].page.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(members[1].page, false);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }

  // Presence gate that works for a camera-OFF peer: the host's grid tile for the
  // guest (located by its display name in `h4.floating-name`). A camera-off peer
  // renders this tile as an avatar with NO canvas, so a canvas-count gate cannot
  // be used here.
  const guestTileOnHost = members[0].page.locator("#grid-container .grid-item", {
    has: members[0].page.locator(`h4.floating-name:has-text("${profiles[1].name}")`),
  });
  await expect(guestTileOnHost).toBeVisible({ timeout: 45_000 });

  return members;
}

/**
 * Open the in-meeting Diagnostics drawer via the "Open Diagnostics" tooltip
 * button (it carries no data-testid). Mirrors
 * `simulcast-per-receiver.spec.ts::openPerformancePanel` /
 * `diagnostics-peer-transport.spec.ts`.
 */
async function openDiagnosticsDrawer(page: Page) {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  return drawer;
}

/**
 * The complete set of device-row labels the panel can emit, in render order
 * (`performance_settings.rs::format_peer_device_lines`). As of issue #1606 the
 * "Main-thread load" row was removed from the user-facing line entirely (it read
 * `0%` for nearly everyone), so it must NEVER appear as a device-row label.
 */
const ALWAYS_AVAILABLE_DEVICE_LABELS = ["OS", "Device", "Cores", "Architecture", "Memory"];

test.describe("Per-peer device / hardware metrics (#1482)", () => {
  // Heavy: two camera-on WebCodecs renderers + a full ~5 s health-interval wait
  // before device fields populate. The three tests below are fully independent —
  // each launches its own two browsers, stands up its own 2-peer call, and tears
  // them down in its own `finally` (no shared state). The project config already
  // sets `fullyParallel: false` + `workers: 2`, so the tests in THIS file run
  // sequentially on a single worker regardless of mode — serial mode added no
  // resource benefit here, and its skip-on-first-failure semantics risked silently
  // skipping the device tests (2 & 3) whenever the unrelated popup test (1) flaked,
  // turning a real validation gap into a false green. Default mode keeps the same
  // within-file sequential execution but lets each test fail and retry independently
  // (`retries: CI ? 2 : 0`). The extended timeout is retained for the heavy setup.
  test.describe.configure({ timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("signal-quality popup shows the peer's compact Device line", async ({ baseURL }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_devmetrics_popup_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      members.push(...(await standUpTwoPeerCall(browsers, uiURL, meetingId)));
      const hostPage = members[0].page;

      // Open the signal-quality popup for the (single) remote peer tile.
      const signalButton = hostPage.locator(
        '#grid-container .canvas-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 15_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // The Device block is omitted until the peer's HealthPacket device fields
      // have been seen (~5 s health interval). Poll through at least one interval.
      // testid is `signal-popup-device-{peer_id}`; with one remote peer there is
      // exactly one such element, so match on the stable prefix.
      const deviceBlock = popup.locator('[data-testid^="signal-popup-device-"]');
      await expect(deviceBlock).toBeVisible({ timeout: 45_000 });
      await expect(deviceBlock).toHaveClass(/\bsignal-popup-device\b/);

      // Head label is the literal "Device".
      await expect(deviceBlock.locator(".signal-popup-device__head")).toHaveText("Device");

      // The compact line is a non-empty dot-separated summary. It must contain at
      // least the always-available Cores token ("N cores") on a Chromium runner —
      // navigator.hardwareConcurrency is never absent — and use the " · "
      // separator the formatter joins with.
      const line = deviceBlock.locator(".signal-popup-device__line");
      await expect(line).toBeVisible();
      await expect(line).toHaveText(/\S/);
      await expect(line).toHaveText(/\d+ cores/);
      await expect(line).toContainText(" · ");
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  test("diagnostics drawer shows the 'Device (per peer)' sub-block", async ({ baseURL }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_devmetrics_diag_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      members.push(...(await standUpTwoPeerCall(browsers, uiURL, meetingId)));
      const hostPage = members[0].page;

      const drawer = await openDiagnosticsDrawer(hostPage);

      // The "Simulcast layers" section must mount (the Device sub-block renders
      // inside it, alongside the per-peer RECEIVE list).
      const simulcastSection = drawer.locator(".diagnostics-section", {
        has: hostPage.getByRole("heading", { name: "Simulcast layers" }),
      });
      await expect(simulcastSection).toBeVisible({ timeout: 30_000 });

      // The Device (per peer) block appears once the peer's HealthPacket device
      // fields have been parsed. It iterates the ALL-PEERS device reader
      // (`all_peer_device_info`), NOT the receive list, so it does not require
      // media flowing — but a camera-on peer trivially satisfies it too. Poll
      // through at least one ~5 s health interval.
      const deviceContainer = drawer.locator("details.diag-device");
      await expect(deviceContainer).toBeVisible({ timeout: 45_000 });
      // issue #1606: the section is a collapsible `<details>` (collapsed by
      // default). Its summary is always visible and reads "Per-peer hardware (N)";
      // expand it before reading the now-hidden rows.
      const deviceSummary = deviceContainer.locator(".diag-disclosure-summary");
      await expect(deviceSummary).toHaveText(/^Per-peer hardware \(\d+\)$/);
      await deviceSummary.click();
      await expect(deviceContainer).toHaveAttribute("open", "");

      // Exactly one remote peer → exactly one per-peer block, keyed by session_id.
      const peerBlock = deviceContainer.locator('[data-testid^="diag-device-peer-"]');
      await expect(peerBlock).toHaveCount(1, { timeout: 30_000 });
      await expect(peerBlock).toHaveClass(/\bdiag-device-peer\b/);

      // The peer label sub-element is present and non-empty (the guest's name).
      const peerLabel = peerBlock.locator(".diag-device-peer-label");
      await expect(peerLabel).toBeVisible();
      await expect(peerLabel).toHaveText(/\S/);

      // At least one label:value row is rendered, and each row exposes both the
      // label and the value span (the row contract).
      const rows = peerBlock.locator(".diag-device-row");
      await expect(rows.first()).toBeVisible({ timeout: 30_000 });
      const rowCount = await rows.count();
      expect(rowCount).toBeGreaterThan(0);

      // Every rendered row carries a non-empty label and a non-empty value, so we
      // never regress to an empty-labeled placeholder row.
      const labels: string[] = [];
      for (let i = 0; i < rowCount; i++) {
        const row = rows.nth(i);
        const label = (await row.locator(".diag-device-row-label").textContent())?.trim() ?? "";
        const value = (await row.locator(".diag-device-row-value").textContent())?.trim() ?? "";
        expect(label.length, `row ${i} label non-empty`).toBeGreaterThan(0);
        expect(value.length, `row ${i} value non-empty`).toBeGreaterThan(0);
        labels.push(label);
      }

      // The "Cores" row is always available on a Chromium runner
      // (navigator.hardwareConcurrency); assert it is one of the rendered labels
      // so the block proves a real device fact, not just structure. The other
      // always-available labels (OS / Device / Architecture / Memory) are a
      // superset we don't gate on individually to avoid runner-specific flake,
      // but every rendered label must be one of the known device-row labels.
      expect(labels).toContain("Cores");
      // issue #1606: "Main-thread load" is NO LONGER an allowed label, so a
      // re-introduced load row would fail this assertion as unexpected.
      const knownLabels = [...ALWAYS_AVAILABLE_DEVICE_LABELS];
      for (const label of labels) {
        expect(knownLabels, `unexpected device-row label "${label}"`).toContain(label);
      }
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // #1482 FOLLOW-UP — the actual fix: a CAMERA-OFF peer's Device block.
  //
  // BEHAVIOR UNDER TEST: the "Device (per peer)" sub-block used to iterate the
  // per-peer RECEIVE list, so a peer appeared there ONLY while media was flowing
  // (camera on). The fix re-points it at `reader.per_peer_device_all` →
  // `client.all_peer_device_info()` (peer_decode_manager.rs::all_peer_device_info),
  // which walks the UNION of live peers + the device-info cache and returns every
  // peer that self-reported non-default device fields via its HealthPacket —
  // INDEPENDENT of media flow.
  //
  // WHY IT WAS BROKEN BEFORE / FIXED NOW: a camera-OFF guest publishes NO media,
  // so on the OLD code it was absent from the receive list → no `.diag-device`
  // block → this test would time out on `.diag-device`. The guest STILL emits
  // HealthPackets with device fields on the ~5 s timer regardless of camera state
  // (health_reporter.rs `start_health_reporting` is gated only on the shutdown
  // flag; device fields come from `read_client_metadata()`, not the camera), so
  // the host's HEALTH arm (video_call_client.rs) parses them and calls
  // `set_peer_device_info`, which inserts into `device_info_cache` UNCONDITIONALLY.
  // On the NEW code `all_peer_device_info` therefore returns the camera-off guest
  // and its block renders — the assertion below passes.
  //
  // PRESENCE GATE: a camera-off peer decodes NO `<canvas>` on the host (its tile
  // renders a `.canvas-container` WITHOUT the `video-on` modifier and with no
  // `<canvas>` child), so `standUpHostAndCameraOffGuest` gates presence on the
  // host's grid TILE for the guest (by display name), not a canvas/decode count.
  // ──────────────────────────────────────────────────────────────────────────
  test("diagnostics drawer shows the Device sub-block for a CAMERA-OFF peer", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_devmetrics_diag_camoff_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      members.push(...(await standUpHostAndCameraOffGuest(browsers, uiURL, meetingId)));
      const hostPage = members[0].page;

      // Sanity: the camera-off guest decodes NO video on the host — nailing the
      // precondition that makes this test prove the fix (the peer is NOT in the
      // receive list, so under the OLD receive-list iteration there would be no
      // Device block at all). A camera-OFF tile still renders a `.canvas-container`
      // element, but WITHOUT the `video-on` modifier and with NO `<canvas>` child
      // (canvas_generator.rs: `show_canvas = is_video_enabled_for_peer &&
      // !force_avatar` gates both the `video-on` class and the `<canvas>`), so we
      // assert on those two decoded-video signals, NOT a bare `.canvas-container`
      // count (which a camera-off peer would still satisfy).
      await expect(hostPage.locator("#grid-container .canvas-container.video-on")).toHaveCount(0, {
        timeout: 15_000,
      });
      await expect(hostPage.locator("#grid-container canvas")).toHaveCount(0, {
        timeout: 15_000,
      });

      const drawer = await openDiagnosticsDrawer(hostPage);

      const simulcastSection = drawer.locator(".diagnostics-section", {
        has: hostPage.getByRole("heading", { name: "Simulcast layers" }),
      });
      await expect(simulcastSection).toBeVisible({ timeout: 30_000 });

      // The crux: even though the guest is camera-OFF (no media, no canvas, not in
      // the receive list), its Device block STILL appears once its HealthPacket
      // device fields have been parsed (~5 s health interval). On the OLD code this
      // container never renders for a camera-off peer — this is the assertion that
      // fails before the fix and passes after it.
      const deviceContainer = drawer.locator("details.diag-device");
      await expect(deviceContainer).toBeVisible({ timeout: 45_000 });
      // issue #1606: collapsible `<details>` — its summary is always visible;
      // expand it before reading the now-hidden per-peer rows.
      const deviceSummary = deviceContainer.locator(".diag-disclosure-summary");
      await expect(deviceSummary).toHaveText(/^Per-peer hardware \(\d+\)$/);
      await deviceSummary.click();
      await expect(deviceContainer).toHaveAttribute("open", "");

      // Exactly one remote peer (the camera-off guest) → exactly one per-peer
      // block, keyed by session_id.
      const peerBlock = deviceContainer.locator('[data-testid^="diag-device-peer-"]');
      await expect(peerBlock).toHaveCount(1, { timeout: 30_000 });
      await expect(peerBlock).toHaveClass(/\bdiag-device-peer\b/);

      // The peer label sub-element is present and non-empty (the guest's name).
      const peerLabel = peerBlock.locator(".diag-device-peer-label");
      await expect(peerLabel).toBeVisible();
      await expect(peerLabel).toHaveText(/\S/);

      // At least one label:value row, each exposing both spans (the row contract).
      const rows = peerBlock.locator(".diag-device-row");
      await expect(rows.first()).toBeVisible({ timeout: 30_000 });
      const rowCount = await rows.count();
      expect(rowCount).toBeGreaterThan(0);

      const labels: string[] = [];
      for (let i = 0; i < rowCount; i++) {
        const row = rows.nth(i);
        const label = (await row.locator(".diag-device-row-label").textContent())?.trim() ?? "";
        const value = (await row.locator(".diag-device-row-value").textContent())?.trim() ?? "";
        expect(label.length, `row ${i} label non-empty`).toBeGreaterThan(0);
        expect(value.length, `row ${i} value non-empty`).toBeGreaterThan(0);
        labels.push(label);
      }

      // "Cores" comes from navigator.hardwareConcurrency — published in the
      // HealthPacket independently of the camera — so it is present even for a
      // camera-off peer. Assert it, and that every rendered label is a known one.
      expect(labels).toContain("Cores");
      // issue #1606: "Main-thread load" is NO LONGER an allowed label.
      const knownLabels = [...ALWAYS_AVAILABLE_DEVICE_LABELS];
      for (const label of labels) {
        expect(knownLabels, `unexpected device-row label "${label}"`).toContain(label);
      }
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
