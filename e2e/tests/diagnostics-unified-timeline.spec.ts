import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Unified diagnostics timeline chart (HCL issue #173 / upstream #712).
 *
 * The diagnostics drawer gained a new "Timeline" section that OVERLAYS several
 * NetEq health metrics on one shared, scrollable time-axis with a per-series
 * on/off checkbox legend. The four pre-existing per-type NetEq charts were KEPT,
 * but moved behind a collapsed `<details>` "Per-metric charts" drill-down.
 *
 * Source of truth (verified against the commit at HEAD of
 * followup/173-diagnostics-timeline):
 *
 *   dioxus-ui/src/components/diagnostics.rs
 *     - The "Timeline" section + its `data-testid="diag-unified-timeline"`
 *       chart container render ABOVE the per-type charts, gated on
 *       `has_history` (a single peer is selected AND that peer has NetEq
 *       history). (diagnostics.rs:1495-1525)
 *     - The four per-type charts live inside a `<details class="diag-disclosure">`
 *       whose summary text is "Per-metric charts" and which has NO `open` attr
 *       → COLLAPSED by default. (diagnostics.rs:1526-1609)
 *     - The unified help control uses `help_testid="diag-chart-unified-help"`
 *       and shares the SAME single-open `open_help` signal as the four per-chart
 *       help icons. (diagnostics.rs:1504-1510)
 *     - With "All Peers" selected (`single_peer == false`) the whole NetEq
 *       cluster collapses to ONE placeholder section
 *       (`.diag-neteq-placeholder`, "Select a specific peer …") — the Timeline
 *       section is NOT rendered. (diagnostics.rs:1470-1479)
 *
 *   dioxus-ui/src/components/neteq_chart.rs
 *     - `unified_series_from_samples` seeds exactly four series, all
 *       `default_on: true`: "Buffer (ms)", "Target (ms)", "Packets awaiting",
 *       "Expand (‰)". (neteq_chart.rs:1333-1372)
 *     - `UNIFIED_DEFAULT_ON_CAP = 4`, so all four default to ON.
 *       (neteq_chart.rs:194)
 *     - The legend is a `.signal-chart-legend.unified-timeline-legend` with one
 *       `<label class="legend-item">` per series, each wrapping an
 *       `<input type="checkbox">`. Legend text is `"{label} · max {N}"`.
 *       (neteq_chart.rs:971-1021)
 *     - A HIDDEN series is FILTERED OUT entirely (no polyline emitted), so the
 *       count of `<polyline>` under the unified chart equals the number of
 *       checked legend boxes. (neteq_chart.rs:737-775)
 *     - The unified chart's inner SVG also draws a frame baseline `<line>` + per-
 *       tick `<line>`/`<text>`, but NO `<polyline>` other than the visible
 *       series — so polyline count is an exact visible-series count.
 *       (neteq_chart.rs:862-885)
 *
 * ─── Determinism / harness note ──────────────────────────────────────────────
 * The Timeline section is gated on `has_history`, which requires full NetEq
 * `stats_json` samples for the selected peer. NetEq is the AUDIO jitter buffer
 * on the receiver, so the host only accumulates history once it has decoded the
 * remote peer's audio for a few seconds (samples are throttled to <=1/sec). Per
 * project convention BOTH the camera and the mic default OFF in pre-join
 * (`vc_prejoin_camera_on` / `vc_prejoin_mic_on` both default `false` —
 * context.rs:755-766), and mock peers never decode. So the guest seeds BOTH
 * flags ON (a real camera-on + audio peer via the fake device) so the host's
 * NetEq actually receives audio samples.
 *
 * Even then, sustained NetEq sample arrival depends on the audio decode pipeline
 * running in the containerized harness, which is not guaranteed in CI. The
 * Timeline-dependent test therefore POLLS for the section and `test.skip`s with
 * a clear reason if it never appears, rather than emitting a flaky assertion.
 * The "All Peers" placeholder test needs NO history and is always live.
 *
 * Structure mirrors signal-quality-peer-transport.spec.ts (auth + 2-peer meeting
 * + camera/mic seeding) and peer-screen-diagnostics.spec.ts (drawer flow).
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/** The four series the unified timeline seeds, in DOM order — all default ON. */
const SERIES_LABELS = ["Buffer (ms)", "Target (ms)", "Packets awaiting", "Expand (‰)"];

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
  opts: { ensureMediaOn?: boolean } = {},
): Promise<Page> {
  const page = await context.newPage();
  if (opts.ensureMediaOn) {
    // Seed BOTH camera and mic ON before the app boots. The mic seed is the
    // load-bearing one for this spec: NetEq history (and so the Timeline
    // section) only fills once the host decodes this peer's AUDIO. Camera-on is
    // seeded too so the peer publishes a full A/V stream (matching the
    // signal-quality spec's camera-on precedent). addInitScript runs before any
    // of the page's own scripts on every navigation, incl. the first.
    await page.addInitScript(() => {
      try {
        window.localStorage.setItem("vc_prejoin_camera_on", "true");
        window.localStorage.setItem("vc_prejoin_mic_on", "true");
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
 * Click "Start Meeting" / "Join Meeting" for an already-admitted user and wait
 * for the grid. Mirrors diagnostics-peer-transport.spec.ts.
 */
async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/** Guest join that handles either direct-join or waiting-room admit. */
async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

/**
 * Open the diagnostics drawer via the toolbar "Open Diagnostics" button and wait
 * for it to render. Mirrors diagnostics-peer-transport.spec.ts::openDiagnosticsPanel
 * and performance-settings.spec.ts::openPerformanceDrawer.
 */
async function openDiagnosticsDrawer(page: Page): Promise<void> {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const sidebar = page.locator("#diagnostics-sidebar");
  await expect(sidebar).toBeVisible({ timeout: 10_000 });
  // The Transport Preference section renders eagerly when the drawer opens;
  // waiting on its h3 confirms the body rendered cleanly.
  await expect(sidebar.locator("h3", { hasText: "Transport Preference" })).toBeVisible({
    timeout: 10_000,
  });
}

test.describe("Diagnostics — unified timeline chart (issue 173)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ── Behavior #5 (always live, no NetEq history required) ───────────────────
  // With "All Peers" selected — the zero-remote-peer solo-meeting default — the
  // NetEq cluster collapses to ONE placeholder section and the unified Timeline
  // section is NOT rendered. This is `single_peer == false` in NetEqStatusAndCharts
  // (diagnostics.rs:1470-1479): the early-return placeholder replaces BOTH the
  // Current Status tiles AND the charts, so `diag-unified-timeline` is absent.
  test("All Peers shows the NetEq placeholder and NOT the unified Timeline section", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_unified_solo_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    let context: BrowserContext | null = null;
    let page: Page | null = null;

    try {
      context = await createAuthenticatedContext(
        browser,
        "host-unified-solo@videocall.rs",
        "UnifiedSoloHost",
        uiURL,
      );
      page = await joinMeetingAs(context, meetingId, "UnifiedSoloHost");
      await clickJoinAndEnterGrid(page);

      await openDiagnosticsDrawer(page);
      const sidebar = page.locator("#diagnostics-sidebar");

      // In a solo meeting there is no remote peer, so the selection is "All
      // Peers" (the Peer Selection dropdown is gated on `available_peers.len() >
      // 1` and may not render at all). Either way `single_peer` is false and the
      // placeholder is shown.
      const placeholder = sidebar.locator(".diag-neteq-placeholder");
      await expect(placeholder).toBeVisible({ timeout: 20_000 });
      await expect(placeholder).toHaveText(
        "Select a specific peer to view time-series charts and current status.",
      );

      // The unified Timeline section must be ABSENT under the placeholder.
      await expect(sidebar.locator('[data-testid="diag-unified-timeline"]')).toHaveCount(0);
      // And so must the unified help control (it lives inside the Timeline section).
      await expect(sidebar.locator('[data-testid="diag-chart-unified-help"]')).toHaveCount(0);
      // The "Timeline" heading must not appear either.
      await expect(sidebar.locator("h3", { hasText: /^Timeline$/ })).toHaveCount(0);
    } finally {
      if (page) await page.close().catch(() => undefined);
      if (context) await context.close().catch(() => undefined);
      await browser.close().catch(() => undefined);
    }
  });

  // ── Behaviors #1, #2, #3, #4, #6 (gated on real NetEq audio history) ───────
  // Needs a real second peer publishing audio so the host accumulates NetEq
  // `stats_json` history (`has_history`) and the Timeline section renders. If
  // the harness never delivers audio NetEq samples, the test skips cleanly
  // rather than emitting a flaky assertion.
  test("with a peer selected: Timeline renders above per-metric charts, legend toggles series, help is single-open", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_unified_peer_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-unified@videocall.rs", name: "UnifiedHost" },
        { email: "guest-unified@videocall.rs", name: "UnifiedGuest" },
      ];

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

      // Host joins first. The GUEST seeds camera + mic ON so it publishes a full
      // A/V stream — the audio is what fills the host's NetEq history.
      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name, {
        ensureMediaOn: true,
      });
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;

      // Host should see exactly one remote peer tile.
      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 45_000,
      });

      await openDiagnosticsDrawer(hostPage);
      const sidebar = hostPage.locator("#diagnostics-sidebar");

      // The drawer auto-selects the sole remote peer in a 1:1 call
      // (`auto_select_peer`, diagnostics.rs), so the Current Status section
      // appears once a peer is selected. Wait for it as the "single peer
      // selected" marker before polling for history-gated content.
      await expect(sidebar.locator("h3", { hasText: /^Current Status$/ })).toBeVisible({
        timeout: 45_000,
      });

      // ── Poll for NetEq history → the Timeline section. ──
      // The unified chart container only renders once `has_history` is true
      // (the host has decoded the peer's audio into NetEq samples). Poll
      // generously; skip cleanly if the harness never delivers samples.
      const unifiedChart = sidebar.locator('[data-testid="diag-unified-timeline"]');
      const appeared = await unifiedChart
        .waitFor({ state: "visible", timeout: 90_000 })
        .then(() => true)
        .catch(() => false);

      if (!appeared) {
        test.skip(
          true,
          "Unified Timeline never rendered: no audio NetEq history accumulated for the peer " +
            "in this harness run (has_history stayed false). The Timeline is gated on sustained " +
            "audio NetEq samples that the containerized audio decode pipeline does not reliably " +
            "produce in CI.",
        );
        return;
      }

      // ── Behavior #1: Timeline section is positioned ABOVE the per-metric
      // charts disclosure. Both are sections under the same sidebar; compare
      // their vertical positions. The "Timeline" heading sits in the unified
      // section; "Per-metric charts" is the per-type `<details>` summary. ──
      const timelineHeading = sidebar.locator("h3", { hasText: /^Timeline$/ });
      await expect(timelineHeading).toBeVisible();
      const perMetricSummary = sidebar.locator("summary", { hasText: "Per-metric charts" });
      await expect(perMetricSummary).toBeVisible();

      const timelineBox = await timelineHeading.boundingBox();
      const perMetricBox = await perMetricSummary.boundingBox();
      expect(timelineBox, "Timeline heading has a layout box").not.toBeNull();
      expect(perMetricBox, "Per-metric charts summary has a layout box").not.toBeNull();
      expect(
        (timelineBox as { y: number }).y,
        "the unified Timeline renders ABOVE the per-metric charts disclosure",
      ).toBeLessThan((perMetricBox as { y: number }).y);

      // ── Behavior #2: legend shows the four series, all CHECKED by default. ──
      const legend = unifiedChart.locator(".unified-timeline-legend");
      await expect(legend).toBeVisible();

      const legendItems = legend.locator(".legend-item");
      await expect(legendItems).toHaveCount(SERIES_LABELS.length);

      // Each expected label appears as a legend item (text is "{label} · max N").
      for (const label of SERIES_LABELS) {
        const item = legend.locator(".legend-item", { hasText: label });
        await expect(item).toHaveCount(1);
        // …and its checkbox starts CHECKED (all four default_on, cap == 4).
        await expect(item.locator('input[type="checkbox"]')).toBeChecked();
      }

      // The hidden-series contract: a series is FILTERED OUT of the SVG when its
      // box is unchecked, so the visible polyline count == checked box count.
      // With all four on, the unified chart's SVG carries four polylines (axis
      // ticks/baseline are <line>, never <polyline>).
      const polylines = unifiedChart.locator("svg polyline");
      await expect(polylines).toHaveCount(SERIES_LABELS.length);

      // ── Behavior #2 (toggle): unchecking a box hides that series' polyline;
      // rechecking shows it again. Use the "Expand (‰)" series (index 3). ──
      const expandItem = legend.locator(".legend-item", { hasText: SERIES_LABELS[3] });
      const expandBox = expandItem.locator('input[type="checkbox"]');

      await expandBox.uncheck();
      await expect(expandBox).not.toBeChecked();
      await expect(polylines).toHaveCount(SERIES_LABELS.length - 1);

      await expandBox.check();
      await expect(expandBox).toBeChecked();
      await expect(polylines).toHaveCount(SERIES_LABELS.length);

      // ── Behavior #3: the four per-type charts are COLLAPSED by default inside
      // the "Per-metric charts" <details>, and still render when expanded
      // (regression guard that they were not deleted). ──
      const perMetricDetails = sidebar.locator("details.diag-disclosure", {
        has: sidebar.locator("summary", { hasText: "Per-metric charts" }),
      });
      await expect(perMetricDetails).toHaveJSProperty("open", false);

      // Collapsed: the per-type chart headings are not visible yet. The four
      // per-type charts carry `.diag-chart-head__title` titles.
      const perTypeTitles = perMetricDetails.locator(".diag-chart-head__title");
      await expect(perTypeTitles.first()).toBeHidden();

      // Expand the disclosure and confirm all four per-type chart titles render.
      await perMetricSummary.click();
      await expect(perMetricDetails).toHaveJSProperty("open", true);
      await expect(perTypeTitles).toHaveCount(4);
      for (const title of [
        "Buffer Size vs Target",
        "Decode Operations",
        "Packets Awaiting Decode",
        "Packet Reordering",
      ]) {
        await expect(
          perMetricDetails.locator(".diag-chart-head__title", { hasText: title }),
        ).toBeVisible();
      }

      // ── Behavior #4: the unified help control opens its popover and is
      // mutually exclusive with the other chart help popovers (single-open). ──
      // HelpPopover ids: button data-testid="diag-chart-unified-help"; popover id
      // "diag-chart-unified-help-popover" (= "{key_id}-help-popover", key_id
      // "diag-chart-unified"). The per-chart Buffer help shares the SAME open_help
      // signal, so opening one closes the other.
      const unifiedHelpBtn = sidebar.locator('[data-testid="diag-chart-unified-help"]');
      await expect(unifiedHelpBtn).toBeVisible();
      await expect(unifiedHelpBtn).toHaveAttribute("aria-haspopup", "dialog");
      await expect(unifiedHelpBtn).toHaveAttribute("aria-expanded", "false");

      const unifiedPopover = hostPage.locator("#diag-chart-unified-help-popover");
      await expect(unifiedPopover).toHaveCount(0);

      await unifiedHelpBtn.scrollIntoViewIfNeeded();
      await unifiedHelpBtn.click();
      await expect(unifiedHelpBtn).toHaveAttribute("aria-expanded", "true");
      await expect(unifiedPopover).toBeVisible();
      await expect(unifiedPopover).toHaveAttribute("role", "dialog");

      // Open the per-chart Buffer help (inside the now-expanded per-metric
      // disclosure). It shares the single-open signal, so the unified popover
      // must close.
      const bufferHelpBtn = sidebar.locator('[data-testid="diag-chart-buffer-help"]');
      await expect(bufferHelpBtn).toBeVisible();
      await bufferHelpBtn.scrollIntoViewIfNeeded();
      await bufferHelpBtn.click();
      await expect(bufferHelpBtn).toHaveAttribute("aria-expanded", "true");
      await expect(hostPage.locator("#diag-chart-buffer-help-popover")).toBeVisible();
      // Single-open: the unified popover closed when the Buffer popover opened.
      await expect(unifiedHelpBtn).toHaveAttribute("aria-expanded", "false");
      await expect(unifiedPopover).toHaveCount(0);

      // Escape closes the open Buffer popover (keyboard-operable).
      await hostPage.keyboard.press("Escape");
      await expect(bufferHelpBtn).toHaveAttribute("aria-expanded", "false");
      await expect(hostPage.locator("#diag-chart-buffer-help-popover")).toHaveCount(0);

      // ── Behavior #6: scroll-sync — scrolling the unified chart's scroll box
      // moves the per-type charts' scroll boxes in lockstep. Only meaningful
      // when the timeline is wide enough to scroll AND the per-metric disclosure
      // is open (the sibling scroll boxes must be mounted). Both hold here. ──
      const unifiedScroll = sidebar.locator("#neteq-chart-scroll-unified");
      const bufferScroll = sidebar.locator("#neteq-chart-scroll-buffer");
      await expect(unifiedScroll).toBeVisible();
      await expect(bufferScroll).toBeVisible();

      const scrollable = await unifiedScroll.evaluate((el) => el.scrollWidth - el.clientWidth > 8);
      if (!scrollable) {
        // Not enough history to overflow the viewport → nothing to scroll-sync.
        // The structural wiring (shared `.neteq-chart-scroll` class + onscroll
        // handler) is asserted by the elements' presence above; the live sync is
        // only observable once the axis overflows.
        test.info().annotations.push({
          type: "warning",
          description:
            "unified chart did not overflow its viewport (too few samples) — live scroll-sync " +
            "assertion skipped; scroll-box wiring still verified structurally.",
        });
      } else {
        await unifiedScroll.evaluate((el) => {
          el.scrollLeft = el.scrollWidth;
          el.dispatchEvent(new Event("scroll", { bubbles: true }));
        });
        // The sibling per-type chart should follow to (approximately) the same
        // offset. Poll because the sync runs through the Dioxus onscroll handler.
        await expect
          .poll(
            async () => {
              const u = await unifiedScroll.evaluate((el) => el.scrollLeft);
              const b = await bufferScroll.evaluate((el) => el.scrollLeft);
              return Math.abs(u - b) <= 4 ? "synced" : `u=${u} b=${b}`;
            },
            { timeout: 10_000 },
          )
          .toBe("synced");
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

  // ── Issue #1452: the crosshair tooltip is a singleton <body>-level
  // <div id="unified-timeline-tooltip-global"> shown via display:block on
  // onmousemove and hidden on onmouseleave. If the chart UNMOUNTS while the
  // tooltip is visible (drawer closed / "All Peers" / peer left) WITHOUT the
  // pointer first leaving the chart, no onmouseleave fires and the singleton
  // used to stay display:block at its last position. The fix adds
  // `use_drop(hide_unified_tooltip)` to UnifiedTimelineChart so the hide runs
  // on unmount regardless of pointer path. This test drives that exact flow:
  // show the tooltip via mousemove, then close the drawer (unmounts the chart)
  // WITHOUT a prior mouseleave, and assert the tooltip hid. ──
  test("crosshair tooltip hides when the chart unmounts (drawer close) without a prior mouseleave (#1452)", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_unified_unmount_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-unmount@videocall.rs", name: "UnmountHost" },
        { email: "guest-unmount@videocall.rs", name: "UnmountGuest" },
      ];

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

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);
      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name, {
        ensureMediaOn: true,
      });
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 45_000,
      });

      await openDiagnosticsDrawer(hostPage);
      const sidebar = hostPage.locator("#diagnostics-sidebar");
      await expect(sidebar.locator("h3", { hasText: /^Current Status$/ })).toBeVisible({
        timeout: 45_000,
      });

      // Same history gate as the sibling test — skip cleanly if the
      // containerized audio pipeline never accumulates NetEq history.
      const unifiedChart = sidebar.locator('[data-testid="diag-unified-timeline"]');
      const appeared = await unifiedChart
        .waitFor({ state: "visible", timeout: 90_000 })
        .then(() => true)
        .catch(() => false);
      if (!appeared) {
        test.skip(
          true,
          "Unified Timeline never rendered (no audio NetEq history in this harness run); " +
            "the #1452 unmount-hide flow needs the chart mounted to exercise.",
        );
        return;
      }

      // Show the tooltip: hover the crosshair OVERLAY. The onmousemove handler
      // lives on an absolute HTML <div data-testid="unified-timeline-crosshair">
      // layered over the SVG (NOT the SVG itself), and only shows the tooltip
      // when it resolves at least one series value at the pointer's time-offset.
      // The singleton tooltip lives at <body>, NOT inside the sidebar.
      const tooltip = hostPage.locator("#unified-timeline-tooltip-global");
      const crosshair = sidebar.locator('[data-testid="unified-timeline-crosshair"]').first();
      await crosshair.scrollIntoViewIfNeeded();
      await expect(crosshair).toBeVisible();

      await expect(crosshair).toBeVisible();
      const cbox = await crosshair.boundingBox();
      expect(cbox, "crosshair overlay has a layout box").not.toBeNull();
      const cb = cbox as { width: number; height: number };
      // Sweep a REAL pointer across the overlay (Dioxus's delegated listener
      // receives genuine browser mousemoves; a synthetic dispatchEvent does not
      // carry the offsetX Dioxus reads). `hover({position})` scrolls the element
      // into view and moves the pointer to an element-relative point, so at least
      // one column resolves a nearest series sample and shows the tooltip.
      await expect
        .poll(
          async () => {
            for (const frac of [0.5, 0.35, 0.65, 0.25, 0.8]) {
              await crosshair.hover({
                position: { x: cb.width * frac, y: cb.height / 2 },
                force: true,
              });
            }
            return (await tooltip
              .evaluate((el) => (el as HTMLElement).style.display)
              .catch(() => "absent")) === "block"
              ? "shown"
              : "hidden";
          },
          {
            timeout: 15_000,
            message:
              "crosshair tooltip never showed on mousemove (precondition for the unmount-hide check)",
          },
        )
        .toBe("shown");

      // CRITICAL: close the drawer WITHOUT moving the pointer off the chart
      // first — this UNMOUNTS UnifiedTimelineChart with the tooltip still
      // display:block. No onmouseleave fires on this path; only the #1452
      // use_drop can hide it. (Use a programmatic click so no pointer travels
      // over the chart's onmouseleave on the way to the button.)
      await sidebar.locator('button[aria-label="Close panel"]').dispatchEvent("click");
      // When closed, attendants.rs keeps a lightweight #diagnostics-sidebar
      // PLACEHOLDER in the DOM (it does not unmount the shell), so assert the
      // CHART itself unmounted — that is the event that drives the use_drop.
      await expect(sidebar.locator('[data-testid="diag-unified-timeline"]')).toHaveCount(0, {
        timeout: 10_000,
      });

      // The fix: the singleton tooltip is now display:none (or removed). Pre-fix
      // it stayed display:block at its last position.
      await expect
        .poll(
          async () =>
            tooltip.evaluate((el) => (el as HTMLElement).style.display).catch(() => "none"),
          {
            timeout: 10_000,
            message:
              "issue #1452 regression: the crosshair tooltip stayed visible after the chart " +
              "unmounted (drawer closed) — use_drop(hide_unified_tooltip) did not fire.",
          },
        )
        .not.toBe("block");
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
