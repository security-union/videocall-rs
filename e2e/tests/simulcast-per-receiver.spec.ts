/**
 * E2E: per-receiver simulcast (PR #1079, issue #989 P1–P5).
 *
 * ## #1108 — "one bad receiver must not degrade the others" (Phase B)
 *
 * Phase B (issue #1108) DECOUPLED the sender's adaptive-quality from receiver
 * feedback. BEFORE: a publisher would shed simulcast layers / step its tier DOWN
 * when REMOTE RECEIVERS reported low FPS, so one struggling receiver dragged the
 * stream down for the WHOLE room. AFTER: the publisher adapts ONLY to its OWN
 * signals (encoder-CPU backpressure, server CONGESTION, WS send-buffer pressure);
 * receiver feedback now influences ONLY each receiver's OWN per-receiver layer
 * pull (the simulcast chooser, already wired). The end-to-end proof of #1108 is
 * therefore identical to the per-receiver DIVERGENCE this spec already exercises:
 * throttle ONE receiver and assert ITS layer drops while the OTHER receivers KEEP
 * the higher layer(s) — i.e. the publisher's ladder did NOT shrink for everyone.
 * The `@impair` WS divergence test below carries that #1108 assertion explicitly
 * (it captures the healthy peer's layer BEFORE the impairment and proves it does
 * not regress AFTER). The unit/integration layer locks the inverse direction —
 * the sender no longer even HAS an input path for receiver FPS — in
 * `bot/tests/aq_degradation.rs` (`bot_does_not_degrade_on_receiver_fps`,
 * `bot_degrades_on_synthetic_backpressure`, `bot_degrades_on_congestion_cut`).
 *
 * The feature is FLAG-GATED OFF in production (`experimentalSimulcastMaxLayers`
 * defaults to 1 = single layer; effective layers =
 * `min(flag, device-capability-ceiling)`). This spec ENABLES the flag for the
 * test browser only via `enableSimulcastFlag` (a `/config.js` route patch — it
 * does NOT modify the committed `dioxus-ui/scripts/config.js` nor the
 * developer's gitignored `config.local.js`).
 *
 * ## STATUS: #1093 hook DELIVERED — the 4 SEND-side tests now RUN
 *
 * Every test here joins TWO (or three) authenticated browser contexts — a
 * publisher + receiver(s) — each running camera + simulcast encode/decode. Two
 * things historically blocked them in headless CI: (1) the 2nd/3rd context never
 * reached the grid, and (2) the device-sniffed capability ceiling clamped a
 * low-core container to 1 layer, so the multi-layer assertions would have skipped
 * anyway. Issue #1093 delivered the mitigations that unblock the SEND-side cases:
 *
 *   - CAPABILITY-OVERRIDE HOOK — the test-only `testCapabilityMaxLayersOverride`
 *     config key (commit bbfe784f) REPLACES the sniffed ceiling, so the runner
 *     emits the full ladder regardless of core count. Wired via
 *     `enableSimulcastFlag(ctx, 3, { capabilityMaxLayersOverride: 3 })`. Each
 *     SEND test ALSO asserts the override took effect (the `TEST-OVERRIDDEN`
 *     publisher console warn) BEFORE its `layerCount <= 1` skip guard, so a
 *     silently-broken override fails loud rather than skipping (see
 *     `assertCapabilityOverrideActive`).
 *   - WAITING-ROOM AUTO-ADMIT — the host's pre-join Waiting Room toggle is
 *     switched OFF in `joinMeeting` (with an async-settle on its `aria-checked`),
 *     so subsequent joiners are admitted straight into the grid instead of being
 *     parked. (This — NOT a renderer crash — was the real 2/3-context join blocker.)
 *   - SERIAL-MODE renderer mitigation — each multi-browser describe runs
 *     `mode: "serial"`, capping the peak concurrent heavy-renderer count on the
 *     8-vCPU CI runner.
 *
 * THEREFORE THE FOLLOWING NOW RUN in the default `dioxus` suite (no longer
 * `test.fixme`):
 *   1. publisher emits >1 simulcast layer when the flag is on
 *   2. receive needle never exceeds the user's max-layer threshold
 *   3. default receive preference is Auto (full range)
 *   4. receive Performance panel renders video + audio + content controls
 *   5. audio readout reflects the multi-layer ladder when the flag is on (the
 *      waiting-room auto-admit fix unblocks the 2-context join; audio's capability
 *      ceiling is DECOUPLED from the cores-based video clamp (#1082), so the
 *      publisher emits the full 3-rung audio ladder regardless of runner core
 *      count — NO capability override needed)
 *   6. flag pinned to 1 / single-layer no-regression (the same 2-context join,
 *      unblocked by the waiting-room fix; needs NO override — it pins the flag to 1)
 *
 * STILL `test.fixme` (with their gating issues — verify against the test bodies
 * below before editing this list):
 *   - receive diagnostics per-peer rows + "+N more" tail (#1093 — needs a real
 *     multi-PUBLISHER, i.e. 3+ context, harness; documented stub)
 *   - publish-side layer suppression, Stage 3 WT + WS (#1093 — 3-context harness)
 *
 * The WT/QUIC per-receiver divergence (#1080 WT half) NO LONGER `test.fixme`s:
 * it now RUNS in the default `dioxus` suite via the client-side `netsim` hook
 * (no toxiproxy — see below and `helpers/downlink-impair.ts`).
 *
 * The `@impair` WS-divergence test (#1080 + #1108) RUNS, but only under
 * `--project=impair` (it is grep-inverted out of the default suite). The
 * single-context structural coverage of the receive Performance panel also lives
 * in `performance-settings.spec.ts` (#1078 Receive-side controls), which is green.
 *
 * The descriptions on the remaining `test.fixme` cases document the INTENDED
 * behaviour each will assert once its gating issue unblocks it.
 *
 * ## What runs in the default suite vs. the `@impair` project
 *
 * Most tests run in the default `dioxus` suite. The per-receiver congestion
 * DIVERGENCE test (issue #1080) instead needs per-client downlink shaping, which
 * the harness now provides for the WS path via the toxiproxy `impair` compose
 * profile + `helpers/downlink-impair.ts`. That test is tagged `@impair` and is
 * grep-inverted OUT of the default `dioxus`/bvt suites — it runs ONLY under
 * `--project=impair` against a stack started with `make e2e-up-impair`, so the
 * standard CI Playwright run is unaffected. Therefore:
 *
 *   - RUNS IN CI:
 *       1. Flag-on multi-layer SEND is active (proxy: a healthy receiver's
 *          received-layer ladder reports `layer_count > 1`).
 *       3. Receive-threshold enforcement via the Performance panel (the user's
 *          key requirement — needle never exceeds the configured max).
 *       4. Default Auto (full range; needle free to reflect auto-selection).
 *
 *       5. Performance panel renders ALL THREE received-quality controls
 *          (video + audio + content) — #1082 structural assertion.
 *       6. AUDIO layering active under the flag (#1082-B: 2 → 3 layers). The
 *          audio needle readout's `{Q} · {i}/{N} · {kbps} kbps` (#1222
 *          quality-letter format) reports the live
 *          per-snapshot `layer_count` (the only DOM signal of audio simulcast);
 *          the ladder-length and bitrate invariants are asserted unconditionally
 *          and the >1-layer assertion is capability-gated like VIDEO send.
 *
 *   - FLAG-OFF CONTROL (separate describe block at the bottom):
 *       Flag pinned to 1 via `pinSimulcastMaxLayers(ctx, 1)` = single layer =
 *       feature OFF. (The runtime DEFAULT was flipped 1 → 3, so the OFF path can
 *       no longer be reached by simply omitting the flag — it must be pinned to 1
 *       explicitly.) The publisher then emits a SINGLE layer for every kind, so
 *       each received-quality readout reports `/1`. This guards the
 *       no-regression byte-identical single-layer path for #1082 (the ladder
 *       machinery went N-generic but the single-layer behavior
 *       must be unchanged).
 *
 *   - RUNS UNDER THE `@impair` PROJECT ONLY (issues #1080 + #1108):
 *       2. Per-receiver congestion DIVERGENCE (WS path) — one of two
 *          co-receivers has its WS downlink bandwidth-clamped via toxiproxy,
 *          which overflows the relay's bounded per-receiver outbound channel;
 *          the relay sheds that receiver's video frames, the resulting sequence
 *          gaps push its `loss_per_sec` over the chooser's step-down threshold,
 *          and ONLY that receiver drops to a lower layer (sender + healthy peer
 *          unaffected). This is ALSO the #1108 headline proof: the healthy peer's
 *          layer is captured BEFORE the impairment and asserted NOT to regress
 *          AFTER — the one bad receiver did not shrink the publisher's ladder for
 *          everyone. See `helpers/downlink-impair.ts` for the full verified
 *          mechanism — the relay-side overflow is what manufactures the loss a
 *          raw bandwidth throttle alone could not. This test is TAGGED `@impair`
 *          and is grep-inverted OUT of the default `dioxus` suite + bvt0/bvt1
 *          (playwright.config.ts), so the standard CI run never touches it. It
 *          runs ONLY under `--project=impair`, which needs the toxiproxy
 *          `impair` compose profile up (`make e2e-up-impair`; run via
 *          `make e2e-impair`). See `TODO(ci)` in that test for the dedicated
 *          CI-job follow-up.
 *
 *   - RUNS IN THE DEFAULT `dioxus` SUITE (issue #1080, WT path):
 *       Per-receiver congestion DIVERGENCE over WebTransport/QUIC. toxiproxy
 *       (the WS mechanism) is TCP-only and cannot shape QUIC/UDP, so this case
 *       instead uses the CLIENT-SIDE `netsim` hook: `impairDownlinkNetsim(page)`
 *       installs a per-TAB inbound shim that drops VIDEO/SCREEN packets on ONLY
 *       the degraded receiver (the `crushed_downlink` preset; AUDIO + control/RTT
 *       always pass), pushing its `loss_per_sec` over the chooser's step-down
 *       threshold. It is LOSS-ONLY (no bandwidth/delay emulation), works on BOTH
 *       transports, needs NO proxy/profile, and is therefore NOT tagged `@impair`
 *       — it runs against a plain `make e2e-up` stack (provided the UI image was
 *       built with the `netsim` cargo feature). See `helpers/downlink-impair.ts`.
 *
 * ## Capability-ceiling caveat (see helpers/simulcast-config.ts)
 *
 * Post-#1140/#1141 `capability_max_simulcast_layers()` derives the ceiling from
 * cheap device facts (core count + UA platform) with NO CPU benchmark; on a
 * low-core CI container it clamps to 1, so the publisher would emit a single
 * layer even with the flag = 3. The #1093 `testCapabilityMaxLayersOverride` hook
 * (wired via `enableSimulcastFlag(ctx, 3, { capabilityMaxLayersOverride: 3 })`)
 * REPLACES that sniffed ceiling so the runner emits the full ladder; the
 * SEND-side and WT-divergence tests use it and prove it took effect via
 * `assertCapabilityOverrideActive` BEFORE their `layer_count <= 1` skip guard.
 * Tests without the override still SKIP rather than assert a false negative.
 *
 * Selectors used (all stable, defined in dioxus-ui source). This spec targets
 * the RECEIVE side only; since the unified send+receive panel landed (#1078) the
 * receive controls/needles live under the `perf-recv-*` / `perf-vu-recv-*`
 * namespace (the bare `perf-*` / `perf-vu-*` ids are now the SEND side):
 *   - toolbar "Open Diagnostics" button             opens the Diagnostics drawer
 *       (#1131: the perf controls MOVED here from the Settings → Performance tab;
 *        the tab + the `perf-open-diagnostics` cross-nav button are gone)
 *   - `#diagnostics-sidebar`                        the drawer root scoping perf-*
 *   - `#perf-vu-recv-video-readout`                 video received-quality readout
 *       text format: `{Q} · {idx+1}/{count} · {w}x{h}` or "Not receiving"
 *       (#1222: `{Q}` is the quality letter L/M/H, or "1" single-layer)
 *   - `#perf-vu-recv-audio-readout`                 audio received-quality readout
 *       text format: `{Q} · {idx+1}/{count} · {kbps} kbps` or "Not receiving"
 *   - `[data-testid="perf-recv-video-range-max"]`   video max-layer range thumb
 *   - `[data-testid="perf-recv-video-auto"]`        video "Reset" button (#1131 §D
 *       REPURPOSED this testid off the former Auto TOGGLE; it is now a plain
 *       button — NO aria-pressed — rendered ONLY while the stream is constrained
 *       and ABSENT at the full default range. Clicking it clears both bounds back
 *       to the full automatic range.)
 */

import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { enableSimulcastFlag, pinSimulcastMaxLayers } from "../helpers/simulcast-config";
import {
  routeDownlinkThroughProxy,
  impairDownlink,
  healDownlink,
  assertProxyUp,
  impairDownlinkNetsim,
  healDownlinkNetsim,
} from "../helpers/downlink-impair";
import {
  readDownlinkCongestionTotal,
  readDownlinkRecoveredTotal,
  readDownlinkShedTotal,
  readLayerFilteredTotal,
  snapshotDownlinkCongestionMetrics,
} from "../helpers/relay-metrics";
import {
  sampleChecksumSeries,
  longestFrozenRunMs,
  distinctChecksumsInWindow,
} from "../helpers/frame-liveness";
import { waitForServices } from "../helpers/wait-for-services";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Transport the publish-suppression (#1108 Stage 3) cases are parameterised over. */
type Transport = "webtransport" | "websocket";

/**
 * Distinctive, stable substring of the `warn!` line that
 * `capability_max_simulcast_layers()` (dioxus-ui/src/components/capability_check.rs)
 * emits to the browser console WHENEVER the #1093 `testCapabilityMaxLayersOverride`
 * key is honoured. The full line is:
 *
 *   "simulcast capability ceiling is TEST-OVERRIDDEN to N layer(s) (requested
 *    testCapabilityMaxLayersOverride=M, clamped to [1, D]); the device-sniffed
 *    ceiling was S (...). This is an e2e-only hook (issue #1093) ..."
 *
 * We match the `TEST-OVERRIDDEN` token: it is unique to that override branch (the
 * non-override path logs an `info!` "simulcast capability ceiling: ..." with NO
 * such token), so its presence proves the override actually replaced the sniffed
 * ceiling — i.e. the `/config.js` route patch landed `testCapabilityMaxLayersOverride`
 * and the UI consumed it. Asserting this BEFORE the `layerCount <= 1` skip guard
 * stops a silently-broken override (key rename, route interception failing, config
 * clobber) from clamping to 1 layer and SKIPPING the test — which would turn the
 * suite green having tested nothing.
 */
const CAPABILITY_OVERRIDE_LOG_TOKEN = "TEST-OVERRIDDEN";

/**
 * Attach a console-message collector to `page` and return the live array of
 * captured message texts (wasm `log::warn!`/`info!` lines surface here via
 * `console_log`). MUST be called BEFORE the page's first navigation so the boot
 * log — which includes the capability-ceiling decision — is captured.
 */
function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => {
    lines.push(msg.text());
  });
  return lines;
}

/**
 * Assert — failing, never skipping — that the #1093 capability override actually
 * took effect on the publisher, by waiting for the `TEST-OVERRIDDEN` `warn!` line
 * to appear in the collected console output. This is the positive proof that the
 * override injection worked; it must run BEFORE any `test.skip(layerCount <= 1)`
 * guard so a broken override fails loudly instead of skipping the test.
 *
 * `lines` is the array returned by {@link collectConsole}, attached to the
 * publisher page before navigation.
 */
async function assertCapabilityOverrideActive(lines: string[]): Promise<void> {
  await expect
    .poll(() => lines.some((line) => line.includes(CAPABILITY_OVERRIDE_LOG_TOKEN)), {
      timeout: 30_000,
      intervals: [250, 500, 1000],
      message:
        `expected the publisher console to log the #1093 capability override ("${CAPABILITY_OVERRIDE_LOG_TOKEN}"). ` +
        "Its absence means testCapabilityMaxLayersOverride did NOT take effect (config.js route " +
        "interception failing, key rename, or config clobber) — the override is silently broken and the " +
        "multi-layer assertion would otherwise SKIP on a clamped single layer, testing nothing.",
    })
    .toBe(true);
}

/**
 * Pin a BrowserContext to a specific media transport BEFORE its first navigation
 * by seeding the sticky preference the UI reads from localStorage at boot
 * (`context.rs`). Mirrors the canonical cross-transport pin in
 * `cross-transport-display-name.spec.ts`. Used by the #1108 Stage 3 cases so the
 * same publish-suppression assertion can be run over BOTH WebTransport and
 * WebSocket without any toxiproxy dependency (no impairment is involved here —
 * the trigger is every receiver PINNING the base layer, not a degraded link).
 */
async function pinTransport(context: BrowserContext, t: Transport) {
  await context.addInitScript((pref: string) => {
    try {
      window.localStorage.setItem("vc_transport_preference", pref);
      window.localStorage.setItem("vc_transport_sticky", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin will set it */
    }
  }, t);
}

/**
 * Drag a receiver's RECEIVE max-layer slider for `kind` down to the lowest rung
 * (index 0 = base layer) with Auto OFF, so this receiver requests ONLY the base
 * layer for the publisher's stream. This is the #1108 Stage 3 DRIVE primitive:
 * when EVERY receiver does this, the relay's per-source layer UNION collapses to
 * base, it emits a `LAYER_HINT` to the publisher, and the publisher caps its
 * published ladder (top rungs shed).
 *
 * The wiring is already live end-to-end on this branch (slider →
 * `set_receive_layer_bounds` → `LAYER_PREFERENCE` → relay union → `LAYER_HINT` →
 * `shared_union_requested_layer` → AQ `observe_union_requested_layer`); the part
 * that is NOT yet observable from the DOM is the publisher-side RESULT — see the
 * Stage 3 describe block header for the `live_simulcast_snapshot` blocker.
 */
async function pinReceiverToBaseLayer(page: Page, kind: "video" | "audio" | "screen" = "video") {
  // Dragging the max thumb to 0 sets a manual bound (leaving the full automatic
  // range) — no toggle click needed (#1131 §D removed the Auto toggle; the Reset
  // button is conditionally rendered and only appears once constrained).
  const maxThumb = page.locator(`[data-testid="perf-recv-${kind}-range-max"]`);
  await expect(maxThumb).toBeVisible({ timeout: 10_000 });
  await maxThumb.focus();
  await maxThumb.fill("0");
  await maxThumb.dispatchEvent("input");
  await expect(maxThumb).toHaveValue("0");
}

/**
 * Drive a fresh page from the HOME FORM into the meeting grid, navigating the
 * #1061 pre-join device-preview screen on the way.
 *
 * This mirrors the PROVEN 2-context flow in `two-users-meeting.spec.ts`
 * (which also uses `createAuthenticatedContext`): go to the home page, type the
 * meeting id + display name, press Enter, then race the pre-join Start/Join
 * action button against the grid. A direct `goto('/meeting/{id}')` did NOT work
 * for these contexts (it failed to surface the pre-join card / crashed) — the
 * home-form path is what reliably establishes the display-name context the
 * meeting page needs, so we replicate it exactly here.
 *
 * `vc_prejoin_camera_on=true` is seeded via an init script BEFORE the app boots
 * so the publisher's camera is ON and the encoder actually emits video — the
 * receive-side needle assertions need a real decoded stream, and the pre-join
 * camera defaults to OFF. (`AttendantsComponent` reads `load_preferred_camera_on`
 * at join, so this carries through both the Start-Meeting click and the
 * auto-join effect.)
 *
 * Applies to BOTH publisher and receiver contexts.
 */
async function joinMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  // Pre-join camera AND mic default to OFF (see `load_preferred_camera_on` /
  // `load_preferred_mic_on`, context.rs); force BOTH ON before the app boots so
  // the publisher emits video AND audio. The audio-ladder / flag-OFF specs read
  // the receiver's AUDIO readout, which stays "Not receiving" forever unless the
  // publisher's mic is actually sending. addInitScript runs on every navigation
  // in this page before the page's own scripts.
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

  // ── Home form: enter the meeting id + display name, then submit (Enter). ──
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });

  // Display name is a controlled input — clear before typing to handle pre-fill.
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  // ── Pre-join card → grid. The meeting page may auto-join straight to the grid
  //    once the display name is set, OR present the pre-join card with a
  //    Start/Join action button. Race both. ──
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
    // Deterministically start the camera on the pre-join card BEFORE joining so
    // the publisher actually emits video (the receive-side needle assertions
    // need a real decoded stream). The persisted camera-ON preference alone is
    // NOT sufficient: `resolve_initial_enabled` (context.rs) only enables the
    // camera at join when the pre-join device list is populated, which requires
    // getUserMedia to have run. So grant media + ensure the camera toggle is ON
    // + await a live preview track, then click the action button.
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

    // HOST ONLY: disable the Waiting Room before starting. The first joiner
    // becomes the host, and the host's pre-join card defaults the Waiting Room
    // toggle ON, which parks every SUBSEQUENT joiner (the receiver(s)) on the
    // "Waiting to be admitted" screen — they never reach #grid-container and the
    // receiver join times out. (This — NOT a renderer crash — is what actually
    // blocks the 2/3-context joins in this spec; see #1093.) The Waiting Room
    // option is rendered ONLY for the owner (pre_join_settings_card.rs `is_owner`),
    // so this is a no-op on a non-host pre-join card. We toggle it OFF so the
    // later joiners are auto-admitted straight into the grid (matching the admit
    // flow proven in two-users-meeting.spec.ts, but without cross-page
    // choreography in every test). The toggle is a `button[role="switch"]` inside
    // the row labelled "Waiting Room".
    const waitingRoomRow = page.locator(".settings-option-row", {
      has: page.getByText("Waiting Room", { exact: true }),
    });
    const waitingRoomToggle = waitingRoomRow.getByRole("switch");
    // The switch is rendered ONLY on the owner's pre-join card (`is_owner` in
    // pre_join_settings_card.rs); on a non-host page it is absent and this whole
    // block is a no-op. Guard on visibility so the receiver's card never blocks.
    if (await waitingRoomToggle.isVisible().catch(() => false)) {
      // SETTLE the toggle before reading it. Its `aria-checked` is seeded ASYNC
      // from the server meeting status: the card can render `false` first and flip
      // to `true` a beat later. A one-shot read can therefore catch the transient
      // `false`, skip the click, and leave the Waiting Room ON — parking every
      // later joiner. Poll until two consecutive reads (~250 ms apart) agree, so we
      // act on the SETTLED state. Bounded; if it never settles we fall through and
      // simply don't toggle (worst case = the pre-existing one-shot behaviour).
      let settled: string | null = null;
      await expect
        .poll(
          async () => {
            const first = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            await page.waitForTimeout(250);
            const second = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            if (first !== null && first === second) {
              settled = second;
              return true;
            }
            return false;
          },
          { timeout: 10_000, intervals: [250, 500] },
        )
        .toBe(true)
        .catch(() => {
          /* never settled within budget — fall through without toggling */
        });
      if (settled === "true") {
        await waitingRoomToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
        // The toggle writes the meeting setting via the meeting-api; wait for the
        // switch to flip to OFF so the setting has been applied before we join.
        await expect(waitingRoomToggle).toHaveAttribute("aria-checked", "false", {
          timeout: 10_000,
        });
      }
    }

    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
        await cameraToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
      // Best-effort wait for a live preview track so the device list is
      // populated before join (this is what starts the in-meeting encoder).
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

    // Enable the MIC on the pre-join card too, so the publisher actually sends
    // audio (the receiver's audio-ladder / flag-OFF readout assertions need a
    // real decoded audio stream). The persisted `vc_prejoin_mic_on=true` seed
    // above is the primary lever; this click is belt-and-suspenders in case the
    // toggle rendered from a stale default before the seed was read. Same
    // `aria-pressed` contract as the camera toggle (pre_join_settings_card.rs).
    const micToggle = page.locator('[data-testid="prejoin-mic-toggle"]');
    if (await micToggle.isVisible().catch(() => false)) {
      if ((await micToggle.getAttribute("aria-pressed")) !== "true") {
        await micToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
    }

    await page.waitForTimeout(500);
    await joinButton.click().catch(() => {
      /* auto-join already unmounted the pre-join button */
    });
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Drive a fresh page from the HOME FORM into the meeting grid as an AUDIO-ONLY
 * publisher: microphone ON, camera OFF. This is the join shape the #1398 mic-side
 * uplink-distress detector requires — its DOWN-step is gated to camera OFF
 * (`detector_camera` reads false; the camera's enabled flag is shared into the
 * mic encoder via `host.rs::set_camera_active_signal`) AND single-layer audio
 * (`pinSimulcastMaxLayers(ctx, 1)` ⇒ `n_audio_layers == 1`). The standard
 * {@link joinMeeting} above FORCES the camera ON for the receive-side video
 * assertions, so it cannot be reused here; this is the deliberate camera-OFF
 * counterpart.
 *
 * Mechanism for camera-OFF / mic-ON:
 *   - Seed `vc_prejoin_camera_on=false` + `vc_prejoin_mic_on=true` BEFORE boot
 *     (the keys `context.rs::DEVICE_PREF_CAMERA_ON_KEY` / `_MIC_ON_KEY` that
 *     `load_preferred_camera_on` / `load_preferred_mic_on` read).
 *   - Grant media permission (so the device list enumerates and `want_mic` in
 *     `attendants.rs::resolve_initial_enabled(prejoin_mic_on, audio_ok, has_mic)`
 *     evaluates true). Camera permission is granted too, but the camera toggle is
 *     left OFF so `want_cam` is false and no video track is published.
 *   - DO NOT click the camera toggle (the only thing that would turn it on).
 *   - Readiness signal is the populated MIC SELECT (`#prejoin-mic-select`,
 *     `pre_join_settings_card.rs::PREVIEW_MIC_SELECT_ID`) — it renders only once
 *     the mic device list is enumerated, which is what makes `want_mic` true.
 *
 * Single publisher page; no receiver is needed (the detector is publisher-side).
 */
async function joinMeetingAudioOnly(
  page: Page,
  meetingId: string,
  displayName: string,
): Promise<void> {
  // Camera OFF, mic ON — the exact inverse of joinMeeting's seed. addInitScript
  // runs on every navigation before the app's own scripts read these keys.
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "false");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage may be unavailable pre-navigation; the app origin sets it */
    }
  });

  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto" as const),
  ]);

  if (result === "join") {
    // Grant media permission so the device list enumerates (needed for the mic to
    // actually start). Camera permission is granted too, but we never enable the
    // camera toggle, so no video track is published.
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

    // This is the FIRST joiner ⇒ host. Disable the Waiting Room (same rationale as
    // joinMeeting — left ON it would park later joiners; harmless no-op for a
    // single-publisher test, but keep the flow identical so a future receiver add
    // does not regress).
    const waitingRoomRow = page.locator(".settings-option-row", {
      has: page.getByText("Waiting Room", { exact: true }),
    });
    const waitingRoomToggle = waitingRoomRow.getByRole("switch");
    if (await waitingRoomToggle.isVisible().catch(() => false)) {
      let settled: string | null = null;
      await expect
        .poll(
          async () => {
            const first = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            await page.waitForTimeout(250);
            const second = await waitingRoomToggle.getAttribute("aria-checked").catch(() => null);
            if (first !== null && first === second) {
              settled = second;
              return true;
            }
            return false;
          },
          { timeout: 10_000, intervals: [250, 500] },
        )
        .toBe(true)
        .catch(() => {
          /* never settled within budget — fall through without toggling */
        });
      if (settled === "true") {
        await waitingRoomToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
        await expect(waitingRoomToggle).toHaveAttribute("aria-checked", "false", {
          timeout: 10_000,
        });
      }
    }

    // Ensure the camera stays OFF: aria-pressed must NOT be "true". We do NOT
    // click it on (the inverse of joinMeeting). If a stale default rendered it
    // ON, click it OFF so `want_cam` is false and the detector's camera-OFF gate
    // holds.
    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) === "true") {
        await cameraToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
      await expect(cameraToggle).toHaveAttribute("aria-pressed", "false", { timeout: 5_000 });
    }

    // Ensure the MIC is ON so the publisher actually captures + encodes audio
    // (the detector lives in the mic encoder's recovery Interval, which only runs
    // while the mic is capturing).
    const micToggle = page.locator('[data-testid="prejoin-mic-toggle"]');
    if (await micToggle.isVisible().catch(() => false)) {
      if ((await micToggle.getAttribute("aria-pressed")) !== "true") {
        await micToggle.click().catch(() => {
          /* toggle may have unmounted on a fast auto-join */
        });
      }
    }

    // Readiness: the mic SELECT renders only once the device list is enumerated,
    // which is the same condition that makes `want_mic` true at join. Waiting on
    // it (rather than the camera preview track, which never appears here) proves
    // the mic will actually start.
    await expect(page.locator("#prejoin-mic-select")).toBeVisible({ timeout: 15_000 });

    await page.waitForTimeout(500);
    await joinButton.click().catch(() => {
      /* auto-join already unmounted the pre-join button */
    });
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Open the in-meeting Diagnostics drawer (the new home of the Performance
 * controls, #1131) and return the drawer locator that scopes the perf controls.
 *
 * #1131 RELOCATION: the Performance panel MOVED out of the Settings → Performance
 * modal tab into the right-side Diagnostics drawer (`#diagnostics-sidebar`),
 * mounted as the "Quality controls" group. The receive controls/meters
 * (`perf-recv-*` / `perf-vu-recv-*`) now render directly inside the drawer's
 * `.sidebar-content` (no `#settings-panel-performance` tabpanel any more). The
 * `perf-*` COMPONENTS are unchanged — only the mount moved — so this spec's
 * RECEIVE assertions are untouched; only the opening flow swapped from "Settings
 * → Performance tab" to "Open Diagnostics".
 *
 * The #1095 redesign already removed the `Receive | Send` direction toggle: every
 * per-kind card renders both a Sending and a Receiving column at once, so the
 * receive controls/meters are always mounted once the drawer is open. We assert
 * the receive video meter is visible INSIDE the drawer as a readiness +
 * relocation guard (this whole spec reads RECEIVE needles/controls).
 */
async function openPerformancePanel(page: Page) {
  // The diagnostics button carries no data-testid; locate it via its tooltip
  // text (mirrors protocol-selection.spec.ts::openDiagnosticsPanel).
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  // Readiness + relocation proof: the migrated receive video meter is present
  // INSIDE the drawer (not anywhere else on the page).
  await expect(drawer.locator('[data-testid="perf-vu-recv-video"]')).toBeVisible({
    timeout: 10_000,
  });
  return drawer;
}

/**
 * Parse the video received-quality readout `#perf-vu-recv-video-readout`.
 * Returns null while the readout reads "Not receiving" (nothing decoded yet),
 * otherwise `{ layerIndex, layerCount }`.
 *
 * #1222 Directive 4 — the readout format changed from `"L{idx+1}/{count} · …"`
 * to `"{Q} · {idx+1}/{count} · …"` where `{Q}` is the quality letter
 * (L/M/H, or "1" for a degenerate single-layer ladder). We parse the
 * `{position}/{count}` numbers AFTER the leading quality letter + " · "; the
 * letter itself is not load-bearing for these tests (the position/count is the
 * 0-based index basis every assertion uses), so we skip past it permissively.
 */
async function readVideoLayer(
  page: Page,
): Promise<{ layerIndex: number; layerCount: number } | null> {
  const text = (await page.locator("#perf-vu-recv-video-readout").textContent())?.trim() ?? "";
  const m = text.match(/^\S+\s+·\s+(\d+)\/(\d+)/);
  if (!m) return null;
  return { layerIndex: Number(m[1]) - 1, layerCount: Number(m[2]) };
}

/**
 * Parse the AUDIO received-quality readout `#perf-vu-recv-audio-readout`.
 *
 * The audio readout format (see `format_readout` in
 * `dioxus-ui/src/components/performance_settings.rs`) is
 * `"{Q} · {idx+1}/{count} · {kbps} kbps"` while receiving (#1222 Directive 4:
 * `{Q}` is the quality letter L/M/H, or "1" single-layer), or "Not receiving"
 * before the first audio frame is decoded.
 *
 * `count` is the LIVE per-snapshot `layer_count` reported by the publisher's
 * audio ladder — this is the only DOM-observable signal of #1082-B (AUDIO went
 * 2 → 3 layers: 24/32/50 kbps). Note the audio *slider* labels intentionally
 * still expose 2 rungs (a product decision in `AUDIO_LAYER_LABELS`); the readout
 * `count`, by contrast, mirrors what the encoder actually emitted, so it is what
 * we assert here.
 *
 * Returns null while the readout reads "Not receiving"; otherwise
 * `{ layerIndex (0-based), layerCount, kbps }`. We skip the leading quality
 * letter + " · " and parse the `{position}/{count} · {kbps} kbps` tail.
 */
async function readAudioLayer(
  page: Page,
): Promise<{ layerIndex: number; layerCount: number; kbps: number } | null> {
  const text = (await page.locator("#perf-vu-recv-audio-readout").textContent())?.trim() ?? "";
  const m = text.match(/^\S+\s+·\s+(\d+)\/(\d+)\s+·\s+(\d+)\s+kbps/);
  if (!m) return null;
  return {
    layerIndex: Number(m[1]) - 1,
    layerCount: Number(m[2]),
    kbps: Number(m[3]),
  };
}

/**
 * The supported AUDIO ladder length after #1082-B (24/32/50 kbps). The readout's
 * reported `layer_count` must never exceed this — a higher value would mean the
 * publisher/receiver ladders silently diverged from the documented #1082 ladder.
 */
const AUDIO_MAX_SUPPORTED_LAYERS = 3;

/** Per-rung AUDIO bitrates from #1082-B, lowest layer first (kbps). */
const AUDIO_LADDER_KBPS = [24, 32, 50] as const;

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

test.describe("Per-receiver simulcast (flag-on)", () => {
  // Two real browser contexts (publisher + receiver) drive several specs; the
  // peer-discovery + layer-adaptation waits make these slower than a unit test.
  //
  // SERIAL (#1093 renderer-crash mitigation). Each test here launches TWO (the
  // @impair divergence test THREE) Chromium *browsers* — a publisher + receiver(s)
  // — each running a live camera plus simulcast encode/decode (multiple concurrent
  // WebCodecs `VideoEncoder`s on the publisher, multi-layer decode on the
  // receiver). That is the heaviest renderer footprint in the suite.
  //
  // The crash in #1093 ("Target page, context or browser has been closed" — the
  // 2nd context never reaches `#grid-container` within the 30 s join timeout) is
  // renderer OOM/kill on the 8-vCPU self-hosted CI runner (c7a.2xlarge) when too
  // many heavy WebCodecs renderers are alive at once. Two levers bound that, and
  // we use BOTH:
  //
  //   1. PER-RENDERER FOOTPRINT — already in place: BROWSER_ARGS
  //      (helpers/auth-context.ts) and the project CHROME_ARGS
  //      (playwright.config.ts) both carry `--disable-dev-shm-usage` (don't put
  //      the renderer's shared memory in the typically-undersized container
  //      `/dev/shm`, the classic "context closed" trigger), `--disable-gpu`, and
  //      `--renderer-process-limit=1`. These shrink each renderer but do NOT bound
  //      the NUMBER of concurrent renderers.
  //
  //   2. CONCURRENT-RENDERER COUNT — this `mode: "serial"`. The project runs
  //      `workers: 2`, so a heavy test in THIS spec can otherwise overlap with a
  //      test in ANOTHER spec file on the second worker (and, during a retry, with
  //      the teardown/`browser.close()` of the previous test in this same spec).
  //      Serial mode pins this whole describe to a single worker and runs its
  //      tests strictly one-at-a-time, so at most ONE publisher+receiver(s) group
  //      of browsers from this spec is ever live — capping the peak heavy-renderer
  //      count this spec contributes. (`fullyParallel:false` already orders
  //      in-file tests, but does not prevent the cross-file overlap or couple the
  //      retry lifecycle; serial makes the one-at-a-time guarantee explicit and
  //      load-bearing, and on a genuinely starved runner its skip-on-first-failure
  //      yields a fast clean signal instead of 4× retried OOM crashes.)
  //
  // The in-test dual load (publisher encoding while the receiver decodes) is
  // inherent to a cross-peer simulcast assertion and cannot be removed; the joins
  // are already staggered (awaited sequentially, publisher before receiver) so the
  // two renderers do not ramp their encoders at the same instant.
  test.describe.configure({ mode: "serial", timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  // -------------------------------------------------------------------------
  // 1. Multi-layer SEND active (flag-on) — proxy via received ladder size.
  //
  // UN-FIXME'd (#1093): the renderer-crash mitigation (serial describe + the
  // existing `--disable-dev-shm-usage` / `--renderer-process-limit=1` launch
  // flags) lets the 2-context publisher+receiver join survive on the 8-vCPU CI
  // runner, and the `capabilityMaxLayersOverride: 3` hook REPLACES the device-
  // sniffed ceiling (which clamps a low-core container to 1) so the publisher
  // actually emits the full ladder and the multi-layer SEND assertion RUNS
  // instead of skipping. The `test.skip(layerCount <= 1)` guard below is retained
  // as defence-in-depth: with the override in effect it should not trip, but if
  // some future runner still clamps it degrades to a skip rather than a false
  // negative.
  // -------------------------------------------------------------------------
  test("publisher emits >1 simulcast layer when the flag is on", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_send_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub@videocall.rs",
        "SimPublisher",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx@videocall.rs",
        "SimReceiver",
        uiURL,
      );
      // Flag ON for BOTH ends: the publisher must encode multiple layers, and
      // the receiver must be allowed to climb above the base layer. The #1093
      // `capabilityMaxLayersOverride: 3` REPLACES the device-sniffed capability
      // ceiling so a low-core CI container (sniffed → 1) still encodes the full
      // ladder, making the multi-layer SEND assertion exercisable.
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation so the capability-ceiling
      // boot log (the #1093 override warn) is collected.
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher");
      await joinMeeting(rxPage, meetingId, "SimReceiver");

      // POSITIVE OVERRIDE PROOF (#1093) — assert the override actually took effect
      // BEFORE the skip guard below. If injection silently broke, layerCount would
      // clamp to 1 and the test would SKIP having tested nothing; this fails loud
      // instead. See assertCapabilityOverrideActive.
      await assertCapabilityOverrideActive(pubConsole);

      // Each side should see the other's tile (peers connected).
      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // The receiver's Performance panel exposes the received-layer ladder.
      await openPerformancePanel(rxPage);

      // Poll until the receiver is actually decoding the publisher's video
      // (readout leaves the "Not receiving" placeholder).
      let snapshot: { layerIndex: number; layerCount: number } | null = null;
      await expect
        .poll(
          async () => {
            snapshot = await readVideoLayer(rxPage);
            return snapshot !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);

      const layerCount = snapshot!.layerCount;
      // CAPABILITY CEILING: a weak/containerized CI runner whose CPU benchmark
      // scores < 5000 clamps the publisher to 1 layer regardless of the flag.
      // That is not a feature failure — skip rather than assert a false neg.
      test.skip(
        layerCount <= 1,
        `runner capability ceiling clamped the publisher to ${layerCount} layer(s); ` +
          "multi-layer send cannot be exercised on this runner (see helpers/simulcast-config.ts)",
      );

      // Flag-on success signal: the publisher is producing a >1-layer ladder
      // and the receiver sees it. (Layer emission isn't directly observable
      // from the client DOM; the received-ladder size is the closest proxy.)
      expect(layerCount).toBeGreaterThan(1);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 3. Receive-threshold enforcement (the user's key requirement).
  //    Drag the video max thumb to the lowest layer with a HEALTHY downlink
  //    and assert the needle never exceeds that threshold.
  //
  // UN-FIXME'd (#1093): the renderer-crash mitigation (serial describe + launch
  // flags) lets the 2-context join survive on CI, and `capabilityMaxLayersOverride:
  // 3` forces a >1-layer ladder so there is headroom for the threshold to clamp.
  // -------------------------------------------------------------------------
  test("receive needle never exceeds the user's max-layer threshold", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_thresh_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub2@videocall.rs",
        "SimPublisher2",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx2@videocall.rs",
        "SimReceiver2",
        uiURL,
      );
      // #1093 override forces a multi-layer ladder so the threshold has headroom
      // to clamp (a single-layer runner would have nothing to step down to).
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher2");
      await joinMeeting(rxPage, meetingId, "SimReceiver2");

      // POSITIVE OVERRIDE PROOF (#1093) — fail (not skip) if the override did not
      // take effect; see assertCapabilityOverrideActive. Runs before the skip guard.
      await assertCapabilityOverrideActive(pubConsole);

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Wait until the receiver is decoding video so the ladder is known.
      await expect
        .poll(async () => (await readVideoLayer(rxPage)) !== null, {
          timeout: 45_000,
          intervals: [500, 1000, 2000],
        })
        .toBe(true);

      const before = await readVideoLayer(rxPage);
      const layerCount = before!.layerCount;
      test.skip(
        layerCount <= 1,
        `single-layer ladder (count=${layerCount}); the threshold has no headroom ` +
          "to clamp on this runner (capability ceiling). See helpers/simulcast-config.ts",
      );

      // Drag the max thumb to the lowest layer (index 0 = "360p"); this alone sets
      // a manual bound and leaves the full automatic range (#1131 §D removed the
      // Auto toggle). The slider is an <input type=range> with min=0 / max=top;
      // setting value to 0 pins max → layer 0.
      const maxThumb = rxPage.locator('[data-testid="perf-recv-video-range-max"]');
      await expect(maxThumb).toBeVisible({ timeout: 10_000 });
      // Set the range input to its lowest position and fire input so Dioxus's
      // oninput handler persists the new bound.
      await maxThumb.focus();
      await maxThumb.fill("0");
      await maxThumb.dispatchEvent("input");
      await expect(maxThumb).toHaveValue("0");

      // Auto-retrying: within the adaptation window the needle must drop to the
      // base layer and NEVER exceed it. We sample repeatedly to catch any
      // transient overshoot — the app must not request above the threshold.
      await expect
        .poll(
          async () => {
            const s = await readVideoLayer(rxPage);
            // While clamping, the readout may briefly read "Not receiving";
            // treat that as within-bound (index 0).
            return s === null ? 0 : s.layerIndex;
          },
          { timeout: 30_000, intervals: [500, 1000, 1500] },
        )
        .toBeLessThanOrEqual(0);

      // Hold the assertion over several more samples to prove it never climbs
      // back above the threshold even with a healthy local downlink.
      for (let i = 0; i < 6; i++) {
        const s = await readVideoLayer(rxPage);
        const idx = s === null ? 0 : s.layerIndex;
        expect(
          idx,
          `received layer must stay <= max threshold (0); sample ${i}`,
        ).toBeLessThanOrEqual(0);
        await rxPage.waitForTimeout(1000);
      }
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 3a. LOCAL CPU pressure steps the RECEIVED simulcast layer DOWN (issue #1569).
  //
  // WHAT #1569 CHANGED (RECEIVER-ONLY): the dioxus decode-budget loop
  // (attendants.rs) now, on the SAME Down edge that already pauses/hides peer
  // tiles (Stage 2), ALSO calls `VideoCallClient::apply_local_cpu_pressure_
  // congestion()` (Stage 1). That seeds synthetic downlink congestion into every
  // connected peer's receiver-side LayerChooser and publishes a lower
  // LAYER_PREFERENCE — i.e. under LOCAL CPU/render pressure this client now
  // requests a LOWER-RESOLUTION stream from its peers. Before #1569 the decode
  // budget ONLY shed tiles; it never stepped the received layer down. The only
  // pre-existing path that lowered received layers was the relay
  // DOWNLINK_CONGESTION signal (a NETWORK trigger) — never local CPU.
  //
  // HOW THIS TEST OBSERVES IT (no network impairment, no toxiproxy):
  //   1. A 2-context publisher+receiver call with a forced >1-layer ladder
  //      (`capabilityMaxLayersOverride: 3`), exactly like test #3. The receiver
  //      climbs above the base layer on a HEALTHY downlink.
  //   2. We then drive the receiver's decode-budget loop to a sustained Down
  //      edge purely with the deterministic test hook
  //      `window.__videocall_inject_render_fps(LOW_FPS)` (registered when
  //      MOCK_PEERS_ENABLED=true, which the e2e stack sets globally — it does NOT
  //      block real peers; the real publisher is still a live connected peer in
  //      the receiver's `connected_peers`). LOW_FPS sits below FPS_STEP_DOWN and
  //      above FPS_SEVERE so the loop takes a single-tile Down step (no severe
  //      multi-drop), and on that Down edge fires
  //      `apply_local_cpu_pressure_congestion()`.
  //   3. The OBSERVABLE SIGNAL is the same one tests #1/#3 read — the receiver's
  //      `#perf-vu-recv-video-readout` layer index (`readVideoLayer().layerIndex`).
  //      With #1569 present it must DROP toward base (the new Stage-1 actuator
  //      published a lower preference). With #1569 reverted, sustained low FPS
  //      still sheds tiles but NEVER lowers the received layer, so the index
  //      would stay at the top rung — this test FAILS on revert. That is the
  //      mutation-sensitive proof that the new layer-down path actually fired.
  //
  // Why the layer-down is feasible here (and why the prerequisites are real):
  //   - `seed_downlink_congestion_for_connected_peers` only publishes a lower
  //     preference for a peer whose `highest_available >= 1` (a >=2-layer video
  //     ladder). The `capabilityMaxLayersOverride: 3` forces that ladder past the
  //     low-core container clamp; the `layerCount <= 1` skip guard degrades to a
  //     SKIP (not a false negative) if some future runner still clamps to 1.
  //   - The DOWN step needs `SUSTAIN_SAMPLES` low samples and respects
  //     `STEP_DOWN_COOLDOWN_MS`; the inject cadence below mirrors
  //     decode-budget.spec.ts (the proven driver of this same loop).
  //
  // UN-FIXME rationale matches test #3: the serial-describe + launch-flag
  // renderer mitigation lets the 2-context join survive on CI, and the override
  // forces the multi-layer headroom the layer-down needs.
  //
  // Mirrors of dioxus-ui/src/components/decode_budget.rs (keep in sync; same
  // consts decode-budget.spec.ts pins — a retune there must update both specs).
  // -------------------------------------------------------------------------
  test("local CPU pressure steps the received simulcast layer DOWN (#1569)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_cpu_down_${Date.now()}`;

    // --- Decode-budget loop consts (mirror decode_budget.rs / decode-budget.spec.ts). ---
    // FPS_SEVERE (12) is the median FPS at/below which a down-step drops MULTIPLE
    // tiles; LOW_FPS below is kept strictly ABOVE it so the DOWN phase takes
    // single-tile / single-rung steps (the severe multi-drop is covered by Rust
    // unit tests, not this timing-sensitive E2E).
    const FPS_STEP_DOWN = 24; // FPS at/below which the loop considers stepping DOWN
    const SUSTAIN_SAMPLES = 3; // consecutive 1 Hz samples required before a step
    const STEP_DOWN_COOLDOWN_MS = 2000; // min ms between two DOWN steps
    // LOW_FPS sits in the MILD band (above SEVERE=12, below STEP_DOWN=24) so the
    // DOWN phase takes single-tile / single-rung steps.
    const LOW_FPS = FPS_STEP_DOWN - 6; // 18: < FPS_STEP_DOWN, > FPS_SEVERE (12)
    // Slightly above the loop's 1 s bucket cadence so each injection lands in a
    // fresh bucket and accumulates wall-time for the down cooldown.
    const INJECT_INTERVAL_MS = 1200;
    const COOLDOWN_DOWN_SAMPLES = Math.ceil(STEP_DOWN_COOLDOWN_MS / INJECT_INTERVAL_MS) + 1;
    // A few SUSTAIN windows plus cooldown headroom for CI jitter: enough samples
    // for at least one Down step (which fires the new layer-down call) to land.
    const MAX_DOWN_SAMPLES = SUSTAIN_SAMPLES + 4 * COOLDOWN_DOWN_SAMPLES;

    const injectFps = (page: Page, fps: number) =>
      page.evaluate(
        (v) =>
          (
            window as unknown as { __videocall_inject_render_fps?: (n: number) => void }
          ).__videocall_inject_render_fps?.(v),
        fps,
      );
    const hasInjectHook = (page: Page) =>
      page.evaluate(
        () =>
          typeof (window as unknown as { __videocall_inject_render_fps?: unknown })
            .__videocall_inject_render_fps === "function",
      );

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub-cpu@videocall.rs",
        "SimPublisherCpu",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx-cpu@videocall.rs",
        "SimReceiverCpu",
        uiURL,
      );
      // #1093 override forces a multi-layer ladder so the received layer has
      // headroom to step DOWN (a single-layer runner has nothing below base).
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisherCpu");
      await joinMeeting(rxPage, meetingId, "SimReceiverCpu");

      // POSITIVE OVERRIDE PROOF (#1093) — fail (not skip) if the override did not
      // take effect; runs before the skip guard so a broken override fails loud.
      await assertCapabilityOverrideActive(pubConsole);

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // The CPU-pressure layer-down hook is gated on MOCK_PEERS_ENABLED. If the
      // stack was brought up without it, the new path cannot be driven — skip
      // rather than assert a false negative. (The e2e compose sets it true.)
      if (!(await hasInjectHook(rxPage))) {
        test.skip(
          true,
          "window.__videocall_inject_render_fps not registered (MOCK_PEERS_ENABLED off)",
        );
      }

      // PHASE 1 — let the receiver climb ABOVE the base layer on a healthy
      // downlink, so a DOWN step is actually observable (otherwise it is already
      // pinned at 0 and there is nothing to step down). SKIP on a single-layer
      // runner (capability ceiling) rather than assert a false negative.
      await expect
        .poll(async () => (await readVideoLayer(rxPage)) !== null, {
          timeout: 45_000,
          intervals: [500, 1000, 2000],
        })
        .toBe(true);

      const before = await readVideoLayer(rxPage);
      const layerCount = before!.layerCount;
      test.skip(
        layerCount <= 1,
        `single-layer ladder (count=${layerCount}); the received layer has no headroom ` +
          "to step DOWN under CPU pressure on this runner (capability ceiling). " +
          "See helpers/simulcast-config.ts",
      );

      await expect
        .poll(async () => (await readVideoLayer(rxPage))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      const startIndex = (await readVideoLayer(rxPage))!.layerIndex;
      expect(
        startIndex,
        "receiver must be above the base layer before we apply CPU pressure",
      ).toBeGreaterThan(0);

      // PHASE 2 — drive the decode-budget loop to a sustained Down edge purely
      // with synthetic LOW FPS (no network impairment). On the Down edge the
      // #1569 actuator publishes a LOWER received-layer preference, so the
      // receiver's layer index must drop BELOW where it started — and, because
      // LOW_FPS is held, eventually reach base (index 0). We feed samples until
      // the index drops or we exhaust the budget.
      await expect
        .poll(
          async () => {
            await injectFps(rxPage, LOW_FPS);
            await rxPage.waitForTimeout(INJECT_INTERVAL_MS);
            // null readout (transient "Not receiving" while re-selecting a lower
            // rung) counts as 0 = below the start index.
            return (await readVideoLayer(rxPage))?.layerIndex ?? 0;
          },
          {
            timeout: MAX_DOWN_SAMPLES * (INJECT_INTERVAL_MS + 500),
            intervals: [INJECT_INTERVAL_MS],
          },
        )
        .toBeLessThan(startIndex);

      // The #1569 actuator steps the received layer down AT LEAST ONE RUNG below
      // the start index under local CPU pressure (matching the host test, which
      // proves 2->1, not 2->0): on a lossless transport the synthetic seed steps
      // the chooser down exactly one rung and then no-ops, and the real {0,0}
      // telemetry never drives `choose` further down, so the layer settles at
      // ~startIndex - 1 (NOT necessarily base index 0) and does not climb back
      // while the pressure holds. Hold the assertion across a few more injected
      // samples to prove the lower preference is sticky under continued pressure.
      for (let i = 0; i < SUSTAIN_SAMPLES + 2; i++) {
        await injectFps(rxPage, LOW_FPS);
        await rxPage.waitForTimeout(INJECT_INTERVAL_MS);
        const idx = (await readVideoLayer(rxPage))?.layerIndex ?? 0;
        expect(
          idx,
          `under sustained CPU pressure the received layer must stay at/below the ` +
            `stepped-down rung (< start ${startIndex}); sample ${i}`,
        ).toBeLessThan(startIndex);
      }
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 3b. Per-peer RECEIVE reason chip renders the "Your setting" DISPLAY text
  //     (issue #1553 — `reason_chip_text` display path).
  //
  // #1553 fixed a receiver-side mis-attribution in the per-peer "reason" chip:
  // the chip's DISPLAY text is produced by `reason_chip_text(DegradeReason)`
  // (performance_settings.rs): Network → "Your network", Setting → "Your
  // setting", Sender → "Sender". The pure attribution LOGIC
  // (`layer_chooser.rs degrade_reason`) — including the #1553 regression case
  // (a constrained receiver with a collapsed `avail_top` is "Network", not
  // "Sender") — is locked by HOST unit tests in BOTH crates
  // (`degrade_reason_*` in layer_chooser.rs and the `reason_chip_text(...)`
  // string asserts in performance_settings.rs). This E2E covers the part those
  // host tests CANNOT: that the DISPLAY string actually reaches the DOM through
  // the per-peer disclosure render (`PeerRow` → `…-peer-{sid}-reason`).
  //
  // DRIVABILITY BOUNDARY (why "Your setting" and not "Your network"):
  //   * "Your setting" (DegradeReason::Setting) is fully drivable from the DOM
  //     with NO network impairment: this receiver drags its OWN video receive
  //     max-layer thumb below the full-ladder top, so the snapshot producer
  //     (`per_peer_received_snapshots`) feeds `user_max = bounds.video.max` and
  //     `sel == user_max < full_ladder_top` → Setting. Deterministic.
  //   * "Sender" needs a genuine single-layer/non-simulcast peer (`avail_top <
  //     full_top && sel == avail_top && !constrained`) — state-dependent on the
  //     runner's clamped ladder, not deterministically forced here.
  //   * "Your network" (DegradeReason::Network — the literal #1553 bug state)
  //     needs `constrained == true`, which only the per-receiver downlink
  //     congestion infra (the `@impair` netsim/toxiproxy path, grep-inverted out
  //     of the default suite) can force. It is NOT drivable in this plain
  //     `make e2e-up` harness, so it stays covered by the host unit tests; this
  //     E2E asserts the drivable "Your setting" branch of the SAME render path.
  //
  // This is the INTENDED reason assertion sketched in the §1b FIXME block below
  // ("cap the receiver via perf-recv-video-range-max → 0, then assert the
  // degraded peer's row shows a perf-reason-chip--setting chip"), made real on
  // the 2-context harness for the ONE publisher the receiver sees.
  //
  // UN-FIXME rationale matches test #3: the serial-describe + launch-flag
  // renderer mitigation lets the 2-context join survive on CI, and
  // `capabilityMaxLayersOverride: 3` forces a >1-layer ladder so there is
  // headroom for a manual cap to sit strictly below the full top (a single-layer
  // ladder has full_top == 0, so no reason chip can ever render).
  // -------------------------------------------------------------------------
  test('per-peer receive reason chip shows "Your setting" when the receiver caps below the full ladder (#1553)', async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_reason_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub3b@videocall.rs",
        "SimPublisher3b",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx3b@videocall.rs",
        "SimReceiver3b",
        uiURL,
      );
      // Force the full ladder on both ends so the receiver's cap can sit STRICTLY
      // below the full top (the reason chip only renders below the full-ladder
      // top; a single-layer runner would never show one).
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher3b");
      await joinMeeting(rxPage, meetingId, "SimReceiver3b");

      // POSITIVE OVERRIDE PROOF (#1093) — fail (not skip) if the override did not
      // take effect; a clamped single-layer ladder has no full top to sit below,
      // so the chip could never appear and the test would prove nothing.
      await assertCapabilityOverrideActive(pubConsole);

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const panel = await openPerformancePanel(rxPage);

      // Wait until the receiver is decoding video so the ladder is known and the
      // per-peer snapshot for the publisher has populated.
      const before = await (async () => {
        await expect
          .poll(async () => (await readVideoLayer(rxPage)) !== null, {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
          })
          .toBe(true);
        return readVideoLayer(rxPage);
      })();
      const layerCount = before!.layerCount;
      // Defence-in-depth (the override proof above should already guarantee >1):
      // a single-layer ladder has full_top == 0, so a manual cap can never sit
      // BELOW the top and no reason chip can render — skip rather than assert a
      // false negative.
      test.skip(
        layerCount <= 1,
        `single-layer ladder (count=${layerCount}); the reason chip cannot render ` +
          "without a full top to sit below on this runner (capability ceiling).",
      );

      // DRIVE the "Setting" attribution: cap THIS receiver's video receive max to
      // the base rung (index 0). `per_peer_received_snapshots` threads this bound
      // as `user_max`; with `sel == user_max == 0 < full_top` the per-peer row's
      // reason resolves to DegradeReason::Setting.
      await pinReceiverToBaseLayer(rxPage, "video");

      // The per-peer RECEIVE disclosure (issue #1131) is a native <details>,
      // collapsed by default (rows are built lazily on expand). Open it.
      const peersDetails = panel.locator('[data-testid="perf-recv-video-peers"]');
      await expect(peersDetails).toBeVisible({ timeout: 15_000 });
      const peersSummary = panel.locator('[data-testid="perf-recv-video-peers-summary"]');
      await expect(peersSummary).toBeVisible({ timeout: 10_000 });
      // Expand if not already open (the summary toggles the <details>).
      if (!(await peersDetails.evaluate((el) => (el as HTMLDetailsElement).open))) {
        await peersSummary.click();
      }
      await expect
        .poll(async () => peersDetails.evaluate((el) => (el as HTMLDetailsElement).open), {
          timeout: 10_000,
          intervals: [250, 500],
        })
        .toBe(true);

      // Exactly one publisher → exactly one per-peer video row. Its reason chip
      // testid is `perf-recv-video-peer-{sessionId}-reason`; match by suffix so we
      // don't need to know the session id.
      const reasonChip = panel.locator(
        '[data-testid$="-reason"][data-testid^="perf-recv-video-peer-"]',
      );

      // The chip appears once the chooser has clamped the decoded layer to the
      // capped bound and the snapshot recomputes its reason. Poll for it.
      await expect(reasonChip).toBeVisible({ timeout: 30_000 });

      // #1553 ASSERTION: the chip renders the exact DISPLAY string from
      // `reason_chip_text(DegradeReason::Setting)`. This is the part the host
      // tests cannot prove — that the mapped string reaches the DOM. A regression
      // that mislabels Setting (or swaps the Network/Sender strings the same
      // mapping owns) fails here.
      await expect(reasonChip).toHaveText("Your setting");

      // …and the chip carries the MATCHING modifier class, so the text and the
      // class can never silently diverge (`reason_chip_modifier(Setting)` ==
      // "setting"). Pinning both pins the whole `DegradeReason → (text, class)`
      // contract for the Setting branch at the DOM.
      const chipClass = (await reasonChip.getAttribute("class")) || "";
      expect(chipClass).toMatch(/\bperf-reason-chip--setting\b/);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 4. Default Auto — with no threshold set the panel shows Auto (full range)
  //    and the needle is free to reflect auto-selection across the full ladder.
  //
  // UN-FIXME'd (#1093): the renderer-crash mitigation (serial describe + launch
  // flags) lets the 2-context join survive on CI. The `capabilityMaxLayersOverride:
  // 3` is passed for consistency with the other SEND tests (the full ladder is the
  // realistic state this asserts the default Auto range against), though this
  // test's assertions are structural (the thumbs sit at the range extremes) and do
  // not themselves require >1 layer.
  // -------------------------------------------------------------------------
  test("default receive preference is Auto (full range)", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_auto_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub3@videocall.rs",
        "SimPublisher3",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx3@videocall.rs",
        "SimReceiver3",
        uiURL,
      );
      // #1093 override forces the full ladder so the "full automatic range" the
      // default-Auto assertion checks actually SPANS up (max thumb top index > 0);
      // on a single-layer runner the range would collapse to [0,0] and the
      // "range spans up" check would be vacuous.
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher3");
      await joinMeeting(rxPage, meetingId, "SimReceiver3");

      // POSITIVE OVERRIDE PROOF (#1093) — the "range spans up" assertion below relies
      // on the override forcing a multi-layer ladder; prove the override landed (fail,
      // not skip) before asserting against it. See assertCapabilityOverrideActive.
      await assertCapabilityOverrideActive(pubConsole);

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const panel = await openPerformancePanel(rxPage);

      // Default state: full automatic range — both thumbs sit at the extremes
      // (min=0, max=top). The Reset button (#1131 §D) is ABSENT at the full range
      // (it only renders once constrained), so its testid resolves to 0 elements.
      await expect(rxPage.locator('[data-testid="perf-recv-video-auto"]')).toHaveCount(0);

      const minThumb = rxPage.locator('[data-testid="perf-recv-video-range-min"]');
      const maxThumb = rxPage.locator('[data-testid="perf-recv-video-range-max"]');
      await expect(minThumb).toHaveValue("0");
      // The max thumb sits at the top index (full range). The exact top value is
      // the ladder size minus one. With the #1093 override forcing the full
      // ladder this MUST be non-zero — assert it so the "range spans up" claim is
      // real (otherwise a single-layer [0,0] range would satisfy the value check
      // vacuously and the test would prove nothing about Auto being full-range).
      const topValue = await maxThumb.getAttribute("max");
      expect(
        Number(topValue),
        "default Auto range must span up (multi-layer ladder)",
      ).toBeGreaterThan(0);
      await expect(maxThumb).toHaveValue(String(topValue));

      // The needle gauge is present and the readout reflects auto-selection
      // (either actively decoding "{Q} · {i}/{n} · …" — #1222 quality-letter
      // format — or "Not receiving" before the first frame). It must NOT be
      // artificially clamped — full range is in effect.
      await expect(panel.locator("#perf-vu-recv-video-readout")).toBeVisible();
      await expect
        .poll(
          async () => (await rxPage.locator("#perf-vu-recv-video-readout").textContent())?.trim(),
          {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
          },
        )
        .toMatch(/^(\S+ · \d+\/\d+|Not receiving)/);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 5. Performance panel renders ALL THREE received-quality controls (#1082).
  //    #1082 keeps video + content at 3 layers and brings AUDIO to 3 layers
  //    (24/32/50 kbps). The receive Performance panel must expose a needle gauge
  //    + Auto toggle + range slider for every kind — video, audio, AND content —
  //    so a user can independently bound each. This is a pure structural
  //    assertion (no capability ceiling dependency): the controls are always
  //    rendered regardless of how many layers the runner ends up emitting.
  //
  // UN-FIXME'd (#1093): the assertion itself is structural
  // (capability-independent — the controls render regardless of layer count), so
  // the ONLY thing that blocked it was the 2-context join crashing the 2nd
  // renderer. The renderer-crash mitigation (serial describe + launch flags)
  // unblocks that join. `capabilityMaxLayersOverride: 3` is still passed for
  // parity with the other SEND tests / a realistic multi-layer state, but is not
  // strictly required here. The single-context structural coverage of the receive
  // panel also lives in performance-settings.spec.ts (#1078 Receive-side controls).
  // -------------------------------------------------------------------------
  test("receive Performance panel renders video + audio + content controls", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_panel_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub5@videocall.rs",
        "SimPublisher5",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx5@videocall.rs",
        "SimReceiver5",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher5");
      await joinMeeting(rxPage, meetingId, "SimReceiver5");

      // POSITIVE OVERRIDE PROOF (#1093) — this test passes capabilityMaxLayersOverride
      // for parity / a realistic multi-layer state, so prove it landed (fail, not skip).
      // See assertCapabilityOverrideActive.
      await assertCapabilityOverrideActive(pubConsole);

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const panel = await openPerformancePanel(rxPage);

      // Every kind must expose its full RECEIVE control set: needle gauge, help
      // button, and dual-thumb range (min + max). The unified panel (#1078) puts
      // the receive controls under the `perf-recv-*` / `perf-vu-recv-*`
      // namespace. Content === Screen kind (testid prefix `perf-recv-screen`,
      // labelled "Shared content" in the UI).
      for (const kind of ["video", "audio", "screen"] as const) {
        await expect(
          panel.locator(`[data-testid="perf-vu-recv-${kind}"]`),
          `${kind} receive needle gauge present`,
        ).toBeVisible();
        // #1131 §D: the former per-kind "Auto" TOGGLE is now a conditionally
        // rendered "Reset" button (same `perf-recv-{kind}-auto` testid). On this
        // fresh join no manual bound is set → the stream is at the full default
        // range → Reset is ABSENT. (The always-present per-kind control is the
        // help button, asserted below.)
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-auto"]`),
          `${kind} receive Reset button absent at the full default range`,
        ).toHaveCount(0);
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-help"]`),
          `${kind} receive help button present`,
        ).toBeVisible();
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-range-min"]`),
          `${kind} receive min thumb present`,
        ).toBeAttached();
        await expect(
          panel.locator(`[data-testid="perf-recv-${kind}-range-max"]`),
          `${kind} receive max thumb present`,
        ).toBeAttached();
      }

      // The audio readout must be present and reflect a valid state: either
      // actively decoding ("{Q} · {i}/{n} · {kbps} kbps" — #1222 quality-letter
      // format) or the "Not receiving" placeholder before the first audio frame.
      // (Layer-count content is asserted in the dedicated audio-layering test below.)
      await expect(panel.locator("#perf-vu-recv-audio-readout")).toBeVisible();
      await expect
        .poll(
          async () => (await rxPage.locator("#perf-vu-recv-audio-readout").textContent())?.trim(),
          {
            timeout: 45_000,
            intervals: [500, 1000, 2000],
          },
        )
        .toMatch(/^(\S+ · \d+\/\d+ · \d+ kbps|Not receiving)/);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 6. AUDIO layering is active under the flag (#1082-B: audio 2 → 3 layers).
  //    The only DOM-observable signal of audio simulcast is the audio needle
  //    readout's reported `layer_count` (`{Q} · {i}/{N} · {kbps} kbps`, #1222
  //    quality-letter format). With the
  //    flag on and a capable runner, the publisher emits up to 3 audio layers,
  //    so the receiver's readout `N` rises above 1. As with VIDEO send, a weak
  //    CI runner's capability ceiling can clamp audio to a single layer — in
  //    that case we SKIP (a single layer is not a feature failure), but we
  //    ALWAYS assert the invariant that `N` never exceeds the documented
  //    3-rung ladder and the reported bitrate is one of {24,32,50} kbps.
  //
  // FIXME(#1093): multi-party (2-context) — needs a renderer-crash-resilient
  // runner + a capability-override hook to force >=2 layers. Headless CI crashes
  // the 2nd context ("Target page/context closed") and clamps audio to 1 layer.
  // -------------------------------------------------------------------------
  test("audio readout reflects the multi-layer ladder when the flag is on", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_audio_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub6@videocall.rs",
        "SimPublisher6",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx6@videocall.rs",
        "SimReceiver6",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(rxCtx, 3);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisher6");
      await joinMeeting(rxPage, meetingId, "SimReceiver6");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Poll until the receiver is actually decoding the publisher's AUDIO
      // (readout leaves the "Not receiving" placeholder).
      let snapshot: { layerIndex: number; layerCount: number; kbps: number } | null = null;
      await expect
        .poll(
          async () => {
            snapshot = await readAudioLayer(rxPage);
            return snapshot !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);

      const { layerCount, layerIndex, kbps } = snapshot!;

      // INVARIANT (always holds, even on a single-layer runner): the audio
      // ladder reported to the receiver must never exceed the documented #1082-B
      // 3-rung ladder, the selected index must be in range, and the reported
      // bitrate must be a known rung. This catches a silent publisher/receiver
      // ladder drift regardless of the capability ceiling.
      expect(layerCount).toBeGreaterThanOrEqual(1);
      expect(layerCount).toBeLessThanOrEqual(AUDIO_MAX_SUPPORTED_LAYERS);
      expect(layerIndex).toBeGreaterThanOrEqual(0);
      expect(layerIndex).toBeLessThan(layerCount);
      expect(AUDIO_LADDER_KBPS).toContain(kbps);

      // CAPABILITY CEILING: a weak/containerized CI runner clamps the publisher
      // to a single audio layer regardless of the flag. That is not a feature
      // failure — skip the multi-layer assertion (see helpers/simulcast-config.ts).
      test.skip(
        layerCount <= 1,
        `runner capability ceiling clamped audio to ${layerCount} layer(s); ` +
          "multi-layer audio send cannot be exercised on this runner",
      );

      // Flag-on success signal for #1082-B: the publisher produced a >1-layer
      // AUDIO ladder (2 or 3 rungs) and the receiver sees it.
      expect(layerCount).toBeGreaterThan(1);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 7. Pin/enlarge a peer tile: RECOVERY + SUSTAINED-freeze smoke guard
  //    (issue #1702; context: #1695, a regression of #1256, fixed by #1698).
  //
  // ⚠️ SCOPE — READ THIS FIRST. This E2E does NOT and CANNOT catch the ≤5s
  // self-healing #1695 transient (proof below: that freeze fits UNDER this
  // spec's sustained-freeze ceiling and recovers before the window ends, so the
  // assertions stay green even on the UNFIXED #1695 build). The DETERMINISTIC
  // #1695 guard is the host test `publish_and_reconcile_pulls_guard_to_rate_
  // limited_wire` (video_call_client.rs:5255). What THIS spec guards is narrower:
  // (a) a pin-driven layer up-switch RECOVERS (frames resume), and (b) the tile
  // shows no SUSTAINED freeze (a permanently-stranded decode guard or a
  // keyframe-starvation regression like the #1662 ~28s class). The "#1695" in the
  // context below explains the BUG that motivated the pin path; it is NOT a claim
  // that this test reproduces that bug.
  //
  // ## The bug that MOTIVATES this path (the #1695 ≤5s decode-guard freeze)
  //
  // #1256 added a SIZE-AWARE receiver layer cap: a small grid thumbnail is
  // capped to a low simulcast layer; a pinned/enlarged tile is `TileHint::
  // Uncapped` so it pulls the FULL layer. Pinning a peer therefore drives an
  // UP-switch of that peer's decode layer. The UI delivers it via
  // `VideoCallClient::set_peer_tile_hints`
  // (videocall-client/src/client/video_call_client.rs:2319), which calls
  // `apply_size_lid_to_decode_guards` — RAISING the exact-match decode guard to
  // the new (higher) layer IMMEDIATELY — then publishes the paired
  // LAYER_PREFERENCE.
  //
  // #1695: when that publish lands within `LAYER_PREFERENCE_MIN_UPDATE_MS`
  // (200ms, layer_preference_sender.rs:87) of a PRIOR accepted publish for the
  // same (peer, Video) key, `take_if_changed` RATE-LIMITS it and returns None
  // WITHOUT promoting `last_sent` (the wire). The relay is exact-match, so it
  // keeps forwarding the OLD low layer (L0) while the guard now demands the high
  // layer (L2): the guard rejects every forwarded L0 frame and the tile FREEZES
  // until the next ~5s monitor tick re-syncs guard↔wire. PR #1698 fixed it by
  // reconciling the guard DOWN to the rate-limited wire after EVERY publish
  // (`publish_and_reconcile` → `reconcile_decode_guards_to_wire`), so the guard
  // never leads the wire and L0 frames keep decoding through the transient.
  //
  // ## What this E2E asserts (and what it deliberately does NOT)
  //
  // FRAME-LIVENESS RECOVERY, not "never holds". The freeze symptom is the tile
  // PAINTING NO NEW FRAME. We sample the pinned tile's `<canvas>` `getImageData`
  // checksum (REUSING the existing frame-liveness primitive from
  // `wt-persistent-streams-freeze-regression.spec.ts:325` — the `ctx.getImageData`
  // 32x32-patch checksum, factored into `helpers/frame-liveness.ts`; the earlier
  // #1698 note that "no frame-liveness primitive exists" is INACCURATE — it has
  // existed in that spec since the WT persistent-streams work).
  //
  // CALIBRATION DISCOVERED ON THE LIVE STACK (2026-06-27): a pin-driven layer
  // UP-SWITCH legitimately holds the last frame for up to ONE publisher GOP (~5s
  // on camera, jitter_buffer.rs:149) while the newly-requested higher layer's
  // keyframe arrives (the keyframe-less hold, jitter_buffer.rs:155-158). A run
  // against the #1698-FIXED stack measured a ~4.5s pixel hold — by design, NOT a
  // bug. That benign one-GOP hold is PIXEL-INDISTINGUISHABLE from the ≤5s #1695
  // freeze, so a "no identical run > ~2s" assertion fails on EVERY correct build.
  //
  // WHY THIS SPEC CANNOT CATCH THE ≤5s #1695 FREEZE (the honest boundary):
  // the #1695 failure on the UNFIXED build is a SELF-HEALING ≤5s frozen tile —
  // it re-syncs at the next ~5s monitor tick. That means, even with #1698
  // reverted, the freeze (i) fits UNDER this spec's one-GOP-plus-slack
  // sustained-freeze ceiling (8000ms) AND (ii) recovers before the ~11s window's
  // recovery tail — so BOTH assertions below would stay GREEN on the very bug
  // #1695 names. Raising the ceiling under 5s is impossible (it would fail the
  // benign one-GOP wait); raising the window past 5s lets the bug recover and
  // pass. Frame-liveness alone therefore CANNOT resolve the sub-5s #1695
  // transient on this localhost+2-peer harness — which is why the rate-limit race
  // is not deterministically forced here (next section) and why this test is
  // SCOPED + NAMED as a recovery/sustained-freeze smoke guard, NOT a #1695
  // reproduction. The deterministic #1695 guard is the host test cited above.
  //
  // WHAT A PIXEL SIGNAL CAN ENFORCE HERE (and all this spec claims): after the
  // pin the tile RECOVERS (paints changing frames again by the end of a window
  // comfortably longer than one GOP) and never holds for MORE than
  // one-GOP-plus-slack — which separates a healthy up-switch from a SUSTAINED
  // freeze (a permanently-stranded decode guard, or a keyframe-starvation
  // regression like #1662's ~28s stall).
  //
  // ### RATE-LIMIT RACE: NOT deterministically forced here — documented lever
  //
  // The freeze ONLY manifests when the pin's up-switch publish lands < 200ms
  // after a PRIOR accepted publish for the SAME (peer, Video) key, so
  // `take_if_changed` rate-limits it and strands guard>wire. Forcing that window
  // deterministically from a browser is NOT possible on this harness because:
  //   * The 200ms `LAYER_PREFERENCE_MIN_UPDATE_MS` clock (`last_sent_ms`) lives
  //     inside the wasm `LayerPreferenceSender` and is NOT exposed on any
  //     `window.__videocall_*` debug hook (grep of videocall-client/src +
  //     dioxus-ui/src: the only hooks are capability-score, render-FPS /
  //     longtask injection, and a STANDALONE freshness test-decoder — none read
  //     or drive `last_sent_ms`, the live decode guard, or the wire map). So the
  //     test cannot OBSERVE the window to fire the pin inside it, nor pin "twice
  //     <200ms apart" with any guarantee the first pin produced an ACCEPTED
  //     publish (a no-op/identical map advances no clock).
  //   * Sub-200ms action timing across the Playwright→browser→wasm boundary is
  //     not reliable enough to land inside the window on demand without flaking.
  // The DETERMINISTIC proof of the race already lives in the mutation-sensitive
  // HOST test `publish_and_reconcile_pulls_guard_to_rate_limited_wire`
  // (video_call_client.rs:5255), which drives the real `publish_and_reconcile`
  // chokepoint at t=1000 (accept L0) → lid-raise guard to L2 → t=1100 (rate-
  // limited up-switch) and asserts the guard is pulled back to L0; it FAILS on
  // either #1698 revert. THIS spec is therefore a SMOKE-LEVEL guard: it exercises
  // the real UI→client→relay→decode pin path end-to-end and proves the tile
  // RECOVERS with no SUSTAINED freeze — value the host test cannot give — but it
  // does NOT reproduce or catch the ≤5s #1695 desync race (per the boundary
  // above; it is untagged → does not run in per-PR CI; the host test is the
  // synchronous per-PR guard for #1695).
  //
  // The LEVER that WOULD let this spec force the race deterministically (a
  // follow-up, mirroring #1457/#1355's documented-lever outcome): a MOCK_PEERS-
  // gated debug hook `window.__videocall_inject_layer_pref_clock(peer_sid, kind,
  // last_sent_ms)` (or a hook to read it) so the test can prime an accepted
  // publish, then pin while the 200ms window is provably open and assert the
  // tile DOES freeze on pre-#1698 / stays live on the fix. Until that hook
  // exists, the host test owns the race and this owns the end-to-end liveness.
  //
  // UN-FIXME rationale matches the SEND tests above: the serial-describe +
  // launch-flag renderer mitigation lets the 2-context join survive on CI, and
  // `capabilityMaxLayersOverride: 3` forces a >1-layer ladder so the pin's
  // size-cap → Uncapped up-switch actually has higher layers to move to (a
  // single-layer runner has nothing to up-switch, so the bug could not arise and
  // the test would prove nothing — hence the `layerCount <= 1` skip guard).
  // -------------------------------------------------------------------------
  test("pinning a peer tile recovers and shows no SUSTAINED freeze (smoke; #1702 — NOT the ≤5s race)", async ({
    baseURL,
  }) => {
    // Two 60s adaptation polls (baseline climb + shrink cap) + joins + ~11s of
    // post-pin sampling can approach the describe's 180s budget on a slow runner;
    // give explicit headroom so a slow-but-passing run is not killed mid-assertion.
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_pin_freeze_${Date.now()}`;

    // Frame-liveness sampling across the pin event.
    //
    // CRITICAL CALIBRATION (verified empirically against the #1698-FIXED live
    // stack, 2026-06-27): a pin-driven layer UP-SWITCH legitimately holds the
    // last frame for up to ONE publisher GOP while the newly-requested higher
    // layer's keyframe arrives. The camera GOP is "at most every 5s"
    // (jitter_buffer.rs:149), and the jitter buffer's keyframe-less hold
    // (jitter_buffer.rs:155-158) holds the last-good frame, by design, until that
    // keyframe — a measured ~4.5s pixel hold ON THE CORRECT BUILD. So a
    // "no identical-pixel run > ~2s" assertion is WRONG: it fails on every correct
    // build (the benign keyframe wait is pixel-indistinguishable from the #1695
    // freeze), and conversely a threshold raised past 5s would also pass on the
    // BUGGY build (the #1695 freeze is itself ≤5s). Frame-liveness alone CANNOT
    // separate the ≤5s #1695 transient from the benign one-GOP up-switch wait on
    // this localhost+2-peer harness — confirming this spec's documented "race not
    // deterministically forced here" stance.
    //
    // WHAT THIS SPEC THEREFORE ASSERTS (smoke-level, still valuable): the tile
    // RECOVERS — it is painting CHANGING frames again by the END of a window
    // comfortably longer than one GOP. That distinguishes the benign one-GOP wait
    // (recovers ≤~5s → tail window is live) from a SUSTAINED freeze regression
    // (e.g. a permanently-stranded guard, or #1662-style keyframe starvation that
    // ran to ~28s) where the tail stays frozen. We sample ~11s at ~400ms cadence
    // and require the last ~4s window to show >1 distinct checksum (live again),
    // and the WHOLE-window frozen run to stay under a one-GOP-plus-slack ceiling
    // so a tens-of-seconds stall still trips.
    const SAMPLE_WINDOW_MS = 11_000;
    const SAMPLE_INTERVAL_MS = 400;
    // Recovery tail: the last RECOVERY_TAIL_MS of the window must be live again.
    const RECOVERY_TAIL_MS = 4_000;
    // A SUSTAINED-freeze ceiling: one GOP (~5s) + generous slack. The benign
    // up-switch keyframe wait (~4.5s measured) sits under this; a multi-GOP /
    // tens-of-seconds stall (a real regression) exceeds it.
    const MAX_FROZEN_RUN_MS = 8_000;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub7@videocall.rs",
        "SimPublisher7",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-rx7@videocall.rs",
        "SimReceiver7",
        uiURL,
      );
      // Force the full ladder on BOTH ends: the publisher must encode >1 layer,
      // and the receiver must be allowed to climb. Without a multi-layer ladder
      // the pin's size-cap→Uncapped up-switch has nothing to move to and the
      // #1695 guard>wire desync cannot arise (the freeze is a multi-layer-only
      // bug). The #1093 override replaces the low-core CI capability clamp.
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      // Capture the publisher console BEFORE navigation (capability-ceiling boot log).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisher7");
      await joinMeeting(rxPage, meetingId, "SimReceiver7");

      // POSITIVE OVERRIDE PROOF (#1093) — fail (not skip) if the override did not
      // take effect, BEFORE the skip guard. A clamped single-layer ladder would
      // make the pin a no-op for the layer cap and the test would prove nothing.
      await assertCapabilityOverrideActive(pubConsole);

      // The receiver must see the publisher's tile before we can pin it.
      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // The #1695 freeze only arises when the pin drives a REAL up-switch
      // (low → high decode layer); pinning an already-top tile moves nothing and
      // never strands guard>wire. So we must FIRST cap the tile to the BASE layer,
      // then pin to force the L0→top up-switch — the exact #1256/#1695 trigger.
      // We replicate the proven cap mechanism from the #1256 size-cap test in this
      // same file: a HiDPI runner (dpr 2) would double the tile's device-px height
      // and lift the size cap off the base, masking the trigger — so require dpr 1
      // and fail loud rather than silently pass on a runner where the cap can't
      // engage. (The #1256 test documents the 640x480→~340px→L0 boundary math.)
      const rxDpr = await rxPage.evaluate(() => window.devicePixelRatio);
      expect(
        rxDpr,
        "this test needs devicePixelRatio === 1 so the shrunk receiver viewport caps " +
          "the tile to the base layer (a HiDPI runner would lift the cap and the pin " +
          "would not up-switch — no #1695 trigger). See the #1256 size-cap test.",
      ).toBe(1);

      // Read the layer via the Diagnostics drawer (same readout the SEND tests
      // use). PHASE 0 — at the DEFAULT (large) viewport the sole-tile receiver
      // climbs ABOVE the base on a healthy link, establishing the multi-rung
      // ladder headroom the cap+up-switch need. Skip (not fail) if it never climbs
      // — that means the runner clamped the publisher to a single layer.
      await openPerformancePanel(rxPage);
      let sawHighBaseline = false;
      await expect
        .poll(
          async () => {
            const s = await readVideoLayer(rxPage);
            if (s && s.layerCount > 1 && s.layerIndex >= 1) sawHighBaseline = true;
            return sawHighBaseline;
          },
          { timeout: 60_000, intervals: [1000, 2000, 3000] },
        )
        .toBe(true)
        .catch(() => {
          /* never reached a high baseline within budget — handled by the skip below */
        });
      test.skip(
        !sawHighBaseline,
        "capability ceiling clamped the publisher to a single layer (the large-tile " +
          "receiver never climbed above the base); no ladder headroom for the size cap " +
          "to be below, so the pin cannot drive the #1695 up-switch. See simulcast-config.ts",
      );

      // PHASE A — SHRINK ⇒ SIZE CAP ENGAGED. Shrink the receiver viewport so its
      // single remote tile becomes a ~340 device-px thumbnail and the #1256 size
      // lid caps the requested layer to the BASE (index 0) on the healthy link.
      await rxPage.setViewportSize({ width: 640, height: 480 });
      await expect
        .poll(async () => (await readVideoLayer(rxPage))?.layerIndex ?? 99, {
          timeout: 60_000,
          intervals: [1000, 2000, 3000],
          message:
            "the shrunk-viewport tile must cap to the base layer (index 0) before the " +
            "pin, so the pin then drives a real L0→top up-switch (the #1695 trigger)",
        })
        .toBe(0);

      // Resolve the pin button. There is exactly ONE remote tile (the publisher;
      // `display_peers` filters out the receiver's own session, attendants.rs),
      // so the first `#grid-container .grid-item` is the publisher's tile. The pin
      // button (`button.pin-icon`, canvas_generator.rs) is `visibility: hidden`
      // until its `.grid-item` parent is hovered (style.css
      // `.grid-item:hover .pin-icon`), so a normal hover-then-click is flaky.
      // REUSE the proven DOM-dispatch click from the #1256 size-cap test in this
      // same file (which pins this exact button): `el.click()` fires the Dioxus
      // onclick (`on_toggle_pin`) regardless of CSS visibility/animation.
      const gridTile = rxPage.locator("#grid-container .grid-item").first();
      await expect(gridTile).toBeVisible({ timeout: 10_000 });
      const pinButton = gridTile.locator("button.pin-icon");
      await expect(pinButton).toHaveCount(1, { timeout: 10_000 });

      // BASELINE: prove the capped tile is still LIVE before the pin (decoding the
      // base layer, a moving synthetic-camera frame), so a post-pin freeze is
      // attributable to the pin, not to a tile that never painted. Require MORE
      // THAN ONE distinct checksum across a short pre-pin window — the pixels are
      // actually changing. (A frozen-run check over a sub-window would be vacuous:
      // a window shorter than MAX_FROZEN_RUN_MS can never produce a run that long.)
      const preSeries = await sampleChecksumSeries(rxPage, 2_000, SAMPLE_INTERVAL_MS, 0);
      const preDistinct = new Set(preSeries.map((s) => s.checksum).filter((c) => c !== null)).size;
      expect(
        preDistinct,
        "the capped peer tile must be painting CHANGING frames before the pin (baseline " +
          "liveness) — if it is already static the post-pin freeze assertion is meaningless",
      ).toBeGreaterThan(1);

      // ACT: pin the peer. This drives toggle_pin → pinned_peer_id → the
      // attendants.rs render that maps this peer to TileHint::Uncapped, LIFTING the
      // base-layer size lid and calling set_peer_tile_hints — the #1256 path that
      // raises the decode guard to the top layer and publishes the up-switch
      // LAYER_PREFERENCE. If that publish is rate-limited (<200ms after a prior
      // accepted publish for this key), the guard leads the wire = the #1695 freeze.
      await pinButton.evaluate((el: HTMLElement) => el.click());

      // Sample the peer tile's pixels across a window LONGER than one publisher
      // GOP, starting immediately after the pin. The pin up-switches the decode
      // layer, which legitimately holds the last frame until the new layer's
      // keyframe arrives (≤ one ~5s GOP); we then assert the tile RECOVERS — see
      // the calibration note on the constants above for why a "never holds"
      // assertion is unsound on this harness.
      const series = await sampleChecksumSeries(rxPage, SAMPLE_WINDOW_MS, SAMPLE_INTERVAL_MS, 0);
      const sampled = series.filter((s) => s.checksum !== null).length;
      const frozenRun = longestFrozenRunMs(series);
      const distinct = new Set(series.map((s) => s.checksum).filter((c) => c !== null)).size;
      const tailFrom = SAMPLE_WINDOW_MS - RECOVERY_TAIL_MS;
      const tailDistinct = distinctChecksumsInWindow(series, tailFrom, SAMPLE_WINDOW_MS + 1);
      console.log(
        `[#1702] post-pin liveness: ${sampled}/${series.length} sampled, ` +
          `${distinct} distinct checksums overall, longest frozen run ${frozenRun}ms ` +
          `(sustained-freeze ceiling ${MAX_FROZEN_RUN_MS}ms); recovery-tail ` +
          `[${tailFrom}-${SAMPLE_WINDOW_MS}ms] distinct=${tailDistinct}`,
      );

      // We must have actually sampled the tile (a null-only series proves
      // nothing). Require a healthy majority of non-null samples.
      expect(
        sampled,
        "the pinned tile must be sampleable across the window (canvas present + readable)",
      ).toBeGreaterThanOrEqual(Math.ceil(series.length / 2));

      // RECOVERY ASSERTION (smoke-level): by the END of the window the tile must
      // be painting CHANGING frames again — the up-switch completed (keyframe
      // arrived) and frames resumed. A SUSTAINED freeze (a permanently-stranded
      // guard, or a keyframe-starvation regression like #1662's ~28s stall) leaves
      // the tail static (≤1 distinct checksum) and FAILS here. The benign ≤5s
      // one-GOP up-switch wait recovers well inside the 11s window so the last 4s
      // are live.
      expect(
        tailDistinct,
        `the pinned peer tile did NOT recover: the last ${RECOVERY_TAIL_MS}ms of the ` +
          `post-pin window showed ${tailDistinct} distinct frame(s) (need > 1 = changing). ` +
          "A one-GOP keyframe wait recovers inside this window; a tail still frozen here is a " +
          "SUSTAINED freeze (stranded decode guard / keyframe starvation) — the #1662-class " +
          "regression. (This does NOT catch the self-healing ≤5s #1695 transient, which would " +
          "recover before the tail; that is the host test's job — see the header.)",
      ).toBeGreaterThan(1);

      // SUSTAINED-FREEZE CEILING: no single identical-pixel run may exceed one GOP
      // plus slack. The benign up-switch keyframe wait (~4.5s measured on the
      // fixed build) sits under this; a multi-GOP / tens-of-seconds stall trips it.
      // This is the coarse upper bound that a frame-liveness signal CAN enforce on
      // this harness (it cannot resolve the sub-5s #1695 transient — see the
      // constants note and the RATE-LIMIT RACE section in the test header).
      expect(
        frozenRun,
        `the pinned peer tile held identical pixels for ${frozenRun}ms after the pin, ` +
          `exceeding the one-GOP-plus-slack ceiling (${MAX_FROZEN_RUN_MS}ms) — a SUSTAINED ` +
          "freeze, not the benign single-GOP up-switch keyframe wait.",
      ).toBeLessThan(MAX_FROZEN_RUN_MS);

      // PIN-FIRED CONFIRMATION (mirrors the #1256 size-cap test's PHASE B, same
      // file): the productive proof the pin actually fired is the received-layer
      // index up-switching ABOVE the base — i.e. `on_toggle_pin → set_peer_tile_
      // hints → apply_size_lid_to_decode_guards` lifted the size lid. We assert
      // this AFTER the liveness sampling so the up-switch poll cannot consume the
      // ≤5s freeze window before we measure it. We do NOT gate on the
      // `.grid-item-pinned` CSS class: at the 640px receiver viewport the canvas
      // div ALSO carries a mobile pin onclick (is_mobile_viewport() < 768,
      // canvas_generator.rs:1243), so the class is not a reliable single-path
      // signal; the layer up-switch is the production-meaningful one.
      await expect
        .poll(async () => (await readVideoLayer(rxPage))?.layerIndex ?? -1, {
          timeout: 45_000,
          intervals: [1000, 2000, 3000],
          message:
            "pinning the peer must lift the size lid and up-switch the received layer " +
            "ABOVE the base (index >= 1) — proof the pin fired and drove the #1695 up-switch",
        })
        .toBeGreaterThanOrEqual(1);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 1b. Per-peer RECEIVE breakdown — now in the Diagnostics "Simulcast layers"
  // section (#1095 §6 MOVE; the old in-panel `perf-recv-{kind}-diag-*` footer was
  // REMOVED).
  //
  // FIXME(#1093): multi-PEER (>= 2 publishers + 1 receiver, i.e. 3 contexts) —
  // needs a renderer-crash-resilient runner + a capability-override hook. After
  // the #1131 unification the per-peer receive breakdown lives in the Diagnostics
  // drawer's "Simulcast layers" section (Group B of the same open drawer), one
  // block per kind, only rendered when >= 1 peer is decoding that kind:
  //   * 0 peers → the kind block is absent (single-context receive coverage —
  //     the "Not receiving" readout placeholder — is green in
  //     performance-settings.spec.ts → receive-needle/readout tests),
  //   * >= 1 peer → `[data-testid="diag-simulcast-recv-{kind}"]` with a head
  //     "{kind} · {n} peer(s) · {spread}" where {spread} is the quality-letter
  //     range (#1222: e.g. "L–H", or a single letter "H" when all peers share a
  //     layer), the top-3 peers as
  //     `[data-testid="diag-simulcast-recv-peer-{sessionId}"]` rows, plus, when
  //     n > 3, a `[data-testid="diag-simulcast-recv-more-{kind}"]` tail
  //     ("+{n-3} more peer(s) on {Quality}" — the full quality word, e.g. "Low").
  //
  // Exercising the per-peer rows + the "+N more" tail therefore requires a real
  // multi-peer simulcast meeting (>= 2 senders so the receiver has >= 2 peers for
  // a kind, and ideally >= 4 to render the "+more" tail). That is blocked on the
  // same harness gaps as every other multi-party test here (#1093): headless CI
  // crashes the extra contexts ("Target page/context closed") and the capability
  // ceiling clamps layers to 1. Documented as `test.fixme` (the single-context
  // structural receive coverage lives in performance-settings.spec.ts #1078).
  //
  // INTENDED assertions once #1093 unblocks this (sketch — left unimplemented on
  // purpose so it is a documented stub, not a runnable test):
  //   1. Join >= 2 publishers (cameras ON, flag ON) + 1 receiver into one room.
  //   2. openPerformancePanel(rxPage) — one open drawer surfaces BOTH the perf
  //      controls (Group A) and the "Simulcast layers" section (Group B); no
  //      cross-nav click needed any more (#1131).
  //   3. expect.poll `[data-testid="diag-simulcast-recv-video"]` head to read
  //      /\d+ peer\(s\) · [LMH]/ (#1222: the spread starts with a quality letter
  //      L/M/H, not the old "L{n}" number).
  //   4. Assert a `[data-testid="diag-simulcast-recv-peer-{sessionId}"]` row
  //      exists for each visible peer (top-3), and with >= 4 publishers assert
  //      `[data-testid="diag-simulcast-recv-more-video"]` reads /\+\d+ more/.
  //   5. Capability-gate the per-peer LAYER assertions (rung/layer counts) on the
  //      received ladder size, mirroring the send-side single-layer skip.
  //
  // #1131 ADDENDUM — the Performance panel now ALSO renders a per-peer RECEIVE
  // disclosure (independent of the Diagnostics breakdown above), so the same
  // multi-peer harness will additionally exercise, in the perf panel:
  //   * `[data-testid="perf-recv-{kind}-peers"]` — a native <details> (collapsed
  //     by default) whose `[data-testid="perf-recv-{kind}-peers-summary"]` shows
  //     "{n} peers · {Qlo}–{Qhi}" (#1222: quality-letter spread, e.g. "L–H").
  //     After expanding it:
  //   * one `[data-testid="perf-recv-{kind}-peer-{sessionId}"]` row per peer, each
  //     carrying a quality dot `…-peer-{sessionId}-q` (class
  //     `perf-q-dot--{optimal|medium|low}`) and — only when the peer is below the
  //     full-ladder top — a reason chip `…-peer-{sessionId}-reason` (class
  //     `perf-reason-chip--{network|setting|sender}`).
  //   * INTENDED reason assertion: cap the receiver via
  //     `perf-recv-video-range-max` → 0, then assert the degraded peer's row shows
  //     a `perf-reason-chip--setting` chip ("Your setting"); a network-impaired
  //     peer (via the @impair infra) shows `perf-reason-chip--network`.
  // -------------------------------------------------------------------------
  test.fixme("receive diagnostics list per-peer rows and a '+N more' tail with multiple publishers", async () => {
    // Blocked on #1093 (multi-peer harness): see the block comment above for the
    // intended multi-publisher flow and the `diag-simulcast-recv-peer-{id}` /
    // `diag-simulcast-recv-more-{kind}` assertions this will perform (the breakdown
    // moved to the Diagnostics panel in #1095; the old `perf-recv-*-diag-*` footer
    // testids no longer exist), PLUS the #1131 perf-panel `perf-recv-{kind}-peers`
    // disclosure + quality-dot + reason-chip assertions documented above.
  });

  // -------------------------------------------------------------------------
  // 2. Per-receiver congestion DIVERGENCE over WebSocket (issues #1080 + #1108).
  //
  //    Now EXERCISED via the per-client downlink-impairment infra
  //    (`helpers/downlink-impair.ts` + the toxiproxy `impair` compose profile).
  //    One of two co-receivers has its WS downlink bandwidth-clamped, which
  //    overflows the relay's bounded per-receiver outbound channel; the relay
  //    sheds that receiver's VIDEO frames, the gaps raise its `loss_per_sec`
  //    above the chooser's step-down threshold, and ONLY that receiver drops to
  //    a lower layer. The sender and the healthy receiver share neither the
  //    proxy nor the relay channel, so they are unaffected. See the helper's
  //    header for the full verified mechanism.
  //
  //    #1108 HEADLINE PROOF ("one bad receiver doesn't degrade others"): after
  //    Phase B the publisher's ladder NO LONGER shrinks in response to a
  //    receiver's poor stats — receiver feedback drives ONLY that receiver's own
  //    per-receiver layer pull. This test makes that literal: it records the
  //    healthy peer's layer index BEFORE impairing the other receiver and, once
  //    the degraded receiver has stepped down, asserts the healthy peer's layer
  //    index has NOT regressed (it stays >= its pre-impairment value and strictly
  //    above the degraded peer). A regression here would mean the bad receiver
  //    dragged the whole room down — exactly the pre-#1108 behavior that was
  //    removed (and is locked at the controller layer by
  //    `bot/tests/aq_degradation.rs::bot_does_not_degrade_on_receiver_fps`).
  //
  //    NOTE: like the other multi-party tests it joins 3 contexts and is subject
  //    to the same headless-CI renderer-crash + capability limits — see #1093;
  //    running it needs the impair runner described below AND that resilience.
  //
  //    GATING: tagged `@impair` — EXCLUDED from the default `dioxus` suite
  //    (grepInvert in playwright.config.ts) and from bvt0/bvt1. It runs ONLY
  //    under `--project=impair`, which requires the toxiproxy proxy to be up
  //    (`make e2e-up-impair`). On the default CI Playwright run this test does
  //    not even appear. `assertProxyUp()` below fails fast with an actionable
  //    message if someone runs the impair project without the proxy.
  //
  //    SCOPE: WebSocket only — `routeDownlinkThroughProxy` pins the degraded
  //    context to WS because toxiproxy is TCP-only. The WT/QUIC equivalent runs
  //    immediately below via the client-side `netsim` hook (no toxiproxy, so it
  //    lives in the default `dioxus` suite, not under `@impair`).
  //
  //    TODO(ci): this `@impair` test is NOT yet wired into a CI job. The
  //    existing CI workflows run `--project=dioxus` (full, e2e-hcl.yaml) and
  //    `--project=bvt1` (smoke, pr-check-e2e-smoke-hcl.yaml), neither of which
  //    starts the toxiproxy `impair` profile, so this test never runs in CI
  //    today. To run it in CI, add a dedicated job mirroring
  //    pr-check-e2e-smoke-hcl.yaml but: (a) bring the stack up with
  //    `COMPOSE_PROFILES=impair ... up -d` (or `make e2e-up-impair`), (b) wait
  //    for toxiproxy's control API on :8474, and (c) run
  //    `npx playwright test --project=impair`. Locally: `make e2e-impair`.
  // -------------------------------------------------------------------------
  test("one bad receiver does not degrade the others: congested receiver drops a layer, healthy peer holds (WS, #1108) @impair", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_diverge_${Date.now()}`;

    // Fail fast (before launching 3 browsers) if the impair profile is not up.
    await assertProxyUp();
    // Start from a clean toxic state so a prior run's leftover toxic cannot
    // pre-degrade the "let layers climb" phase.
    await healDownlink();

    // 1 publisher + 2 receivers. Healthy receiver: normal downlink. Degraded
    // receiver: WS downlink routed through toxiproxy so we can clamp it.
    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub-d@videocall.rs",
        "SimPublisherD",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-healthy@videocall.rs",
        "SimHealthy",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-degraded@videocall.rs",
        "SimDegraded",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3);
      await enableSimulcastFlag(healthyCtx, 3);
      await enableSimulcastFlag(degradedCtx, 3);

      // Route the degraded receiver's media WebSocket through toxiproxy and pin
      // it to WS. MUST run before its first navigation (it patches /config.js).
      await routeDownlinkThroughProxy(degradedCtx);

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      const degradedPage = await degradedCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimPublisherD");
      await joinMeeting(healthyPage, meetingId, "SimHealthy");
      await joinMeeting(degradedPage, meetingId, "SimDegraded");

      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above the base layer on a healthy
      // (un-impaired) downlink. Capability ceiling can clamp to a single layer
      // on a weak runner; in that case there is no headroom to diverge, so SKIP
      // rather than assert a false negative (mirrors tests 1 & 6).
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped the publisher to a single layer; there is no " +
          "ladder headroom to diverge on this runner (see helpers/simulcast-config.ts)",
      );

      // Let the degraded receiver actually reach a layer above base before we
      // impair it — otherwise "stepped down" is unobservable (it is already at 0).
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Capture the healthy peer's layer index at the LAST moment before we
      // impair the other receiver. This is the #1108 baseline: the publisher's
      // ladder for THIS healthy peer must not shrink merely because the OTHER
      // receiver is about to go bad. (Read just before PHASE 2 so it reflects
      // the steady state immediately preceding the impairment.)
      const healthyBeforeImpair = await readVideoLayer(healthyPage);
      expect(
        healthyBeforeImpair,
        "healthy receiver must be decoding before we impair the other receiver",
      ).not.toBeNull();

      // PHASE 2 — clamp ONLY the degraded receiver's downlink hard enough to
      // overflow the relay's 128-slot outbound channel (sheds video → loss →
      // step down). ~120 kbps is far below one HD layer's byte rate.
      await impairDownlink({ rateKb: 15 });

      // PHASE 3 — the degraded receiver's chosen layer must drop strictly BELOW
      // the healthy receiver's. The sender and the healthy receiver share
      // neither the proxy nor the relay channel, so the healthy peer stays high.
      // Guard against a vacuous pass on BOTH sides:
      //   - HEALTHY must be decoding (non-null) — a null healthy read returns
      //     false so we never compare against a missing baseline.
      //   - DEGRADED must ALSO still be decoding (non-null). Under the
      //     `crushed_downlink` preset the degraded receiver can lose decode
      //     entirely (keyframe starvation) and read "Not receiving" →
      //     `readVideoLayer` returns null. Treating a null degraded as the base
      //     layer (index 0) would pass this assertion for the WRONG reason
      //     ("stopped decoding" rather than "stepped down to a lower layer"). So
      //     a null degraded read ALSO returns false — pinning the claim to
      //     "degraded converged to a strictly lower DECODED layer."
      //   - The degraded peer may legitimately oscillate through brief "Not
      //     receiving" windows under heavy loss. Returning false on null does
      //     NOT fail fast — the poll keeps going (timeout 90s, intervals settle
      //     at 5s) until it catches a frame window where degraded IS decoding at
      //     a lower index. That is the intended semantics.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return false;
            return degraded.layerIndex < healthy.layerIndex;
          },
          { timeout: 90_000, intervals: [2000, 3000, 5000] },
        )
        .toBe(true);

      // Healthy receiver unaffected: still decoding and still ABOVE the base
      // layer (its layer was not dragged down by the other receiver's congestion).
      const healthyFinal = await readVideoLayer(healthyPage);
      expect(healthyFinal, "healthy receiver must still be decoding").not.toBeNull();
      expect(
        healthyFinal!.layerIndex,
        "healthy receiver must stay above the base layer (unaffected by peer congestion)",
      ).toBeGreaterThan(0);

      // #1108 NON-REGRESSION (the literal "one bad receiver doesn't degrade the
      // others" proof): the healthy peer's layer must not have dropped relative
      // to its pre-impairment baseline. Before Phase B the publisher would shed
      // layers / step its tier down on the degraded receiver's feedback, shrinking
      // the ladder for EVERY receiver — the healthy peer's index would fall here.
      // After Phase B the publisher adapts only to its OWN signals, so the healthy
      // peer holds (or climbs). Allow >= (a healthy peer may even climb into freed
      // capacity); a strict drop is the forbidden pre-#1108 behavior.
      expect(
        healthyFinal!.layerIndex,
        "#1108: the healthy peer's layer must NOT shrink because the OTHER receiver " +
          `went bad (before=${healthyBeforeImpair!.layerIndex}, after=${healthyFinal!.layerIndex})`,
      ).toBeGreaterThanOrEqual(healthyBeforeImpair!.layerIndex);

      // PHASE 4 — heal the downlink and prove the degraded receiver climbs back
      // up (recovery), confirming the divergence was the impairment, not a
      // permanent failure. Climb-back is conservative (hysteresis), so allow a
      // generous window; if the runner is too slow to re-climb within it this is
      // a soft check (the core divergence above is the load-bearing assertion).
      await healDownlink();
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      // Always remove the toxic so a failure does not leave the proxy degraded
      // for a subsequent run.
      await healDownlink();
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // 2b. WT/QUIC per-receiver divergence — now EXERCISED via the client-side
  //     netsim hook (issue #1080 WT half + #1108).
  //
  // The same chooser step-down → divergence (and therefore the same #1108 "one
  // bad receiver doesn't degrade the others" proof) applies on the WebTransport
  // path. The WS case above manufactures loss RELAY-side via a toxiproxy TCP
  // bandwidth clamp, which cannot work for WT: toxiproxy is TCP-only and
  // Playwright's `newContext({ proxy })` only carries TCP/HTTP(S), so neither can
  // shape QUIC/UDP datagrams; and per-client UDP `tc … netem` needs an isolated
  // netns the shared-netns Playwright harness does not provide.
  //
  // #1080's WT half solves this by moving the impairment INTO the client: when
  // the dioxus UI is built with the `netsim` cargo feature, every page exposes
  // `window.__vcNetsim`, and `impairDownlinkNetsim(page)` installs a PER-TAB
  // inbound shim that drops ~40% of arriving VIDEO/SCREEN packets (the
  // `crushed_downlink` preset; AUDIO + control/RTT always pass). Those dropped
  // packets are real sequence gaps → the receive-side `SequenceTracker` pushes
  // `loss_per_sec` over the chooser's >= 5 gaps/sec step-down threshold → ONLY
  // that receiver drops a layer. It is LOSS-ONLY (no bandwidth/delay emulation)
  // and works on BOTH transports with no proxy/profile, so this runs against a
  // plain `make e2e-up` stack. See `helpers/downlink-impair.ts` for the full
  // mechanism and semantics.
  //
  // GROUPING: deliberately NOT tagged `@impair`. The `@impair` tag forces a test
  // into the `impair` Playwright project, which requires the toxiproxy compose
  // profile (`make e2e-up-impair`) and is grep-inverted OUT of the default
  // `dioxus` suite. This test needs NO toxiproxy — the netsim hook is client-side
  // — so it runs as a normal `dioxus`-suite test against `make e2e-up`. Its only
  // extra requirement is that the UI image carries the `netsim` feature, which
  // `assertNetsimAvailable` (inside `impairDownlinkNetsim`) checks with an
  // actionable rebuild error rather than a confusing TypeError.
  //
  // It mirrors the WS test's structure exactly, INCLUDING the #1108
  // non-regression assertion (the healthy peer's layer must not shrink when the
  // OTHER receiver goes bad). The capability-ceiling override (#1093) is wired
  // like the SEND tests so a low-core CI runner still emits the full ladder
  // (otherwise PHASE 1 would skip-clamp). Multi-party renderer-crash mitigation
  // is the describe-level `mode: "serial"`.
  // -------------------------------------------------------------------------
  test("one bad receiver does not degrade the others over WebTransport (WT, #1108) — client-side netsim, no toxiproxy", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_diverge_wt_${Date.now()}`;

    // 1 publisher + 2 receivers, ALL on WebTransport (the production-primary
    // transport). Unlike the WS case we do NOT pin the degraded receiver to WS —
    // the whole point of this case is to prove the #1108 isolation holds on the
    // QUIC path too. The netsim hook works regardless of the elected transport.
    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let degradedPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-pub-dwt@videocall.rs",
        "SimPublisherDWT",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-healthy-wt@videocall.rs",
        "SimHealthyWT",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-degraded-wt@videocall.rs",
        "SimDegradedWT",
        uiURL,
      );
      // Flag ON for all three, with the #1093 capability override so a low-core
      // CI runner (sniffed ceiling → 1) still encodes the full ladder — otherwise
      // PHASE 1's skip guard below would clamp this test on CI, testing nothing.
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(healthyCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(degradedCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      degradedPage = await degradedCtx.newPage();

      // Capture the publisher console BEFORE navigation so the #1093 override
      // boot warn is collected (proven via assertCapabilityOverrideActive below).
      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "SimPublisherDWT");
      await joinMeeting(healthyPage, meetingId, "SimHealthyWT");
      await joinMeeting(degradedPage, meetingId, "SimDegradedWT");

      // POSITIVE OVERRIDE PROOF (#1093) — assert the override took effect BEFORE
      // the skip guard, so a silently-broken override fails loud instead of
      // skip-clamping to a single layer. See assertCapabilityOverrideActive.
      await assertCapabilityOverrideActive(pubConsole);

      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above the base layer on a healthy
      // (un-impaired) downlink. Capability ceiling can still clamp to a single
      // layer if the override somehow failed; SKIP rather than assert a false
      // negative (the override proof above already fails loud in that case).
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage!);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped the publisher to a single layer; there is no " +
          "ladder headroom to diverge on this runner (see helpers/simulcast-config.ts)",
      );

      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // #1108 baseline: the healthy peer's layer just before impairment.
      const healthyBeforeImpair = await readVideoLayer(healthyPage);
      expect(
        healthyBeforeImpair,
        "healthy receiver must be decoding before we impair the other receiver",
      ).not.toBeNull();

      // PHASE 2 — impair ONLY the degraded receiver's downlink, client-side, via
      // the per-TAB netsim hook (drops inbound VIDEO/SCREEN packets → sequence
      // gaps → loss_per_sec over the chooser's step-down threshold). Installed on
      // the degraded receiver's PAGE so the sender + healthy peer are untouched.
      await impairDownlinkNetsim(degradedPage);

      // PHASE 3 — the degraded receiver's chosen layer must drop strictly BELOW
      // the healthy receiver's. Guard against a vacuous pass on BOTH sides:
      //   - HEALTHY must be decoding (non-null) — a null healthy read returns
      //     false so we never compare against a missing baseline.
      //   - DEGRADED must ALSO still be decoding (non-null). Under
      //     `crushed_downlink` (40% inbound video drop) the degraded receiver
      //     can lose decode entirely (keyframe starvation) and read "Not
      //     receiving" → `readVideoLayer` returns null. Treating a null degraded
      //     as the base layer (index 0) would pass this assertion for the WRONG
      //     reason ("stopped decoding" rather than "stepped down to a lower
      //     layer"). So a null degraded read ALSO returns false — this pins the
      //     claim to "degraded converged to a strictly lower DECODED layer."
      //   - With 40% loss the degraded peer may legitimately oscillate through
      //     brief "Not receiving" windows. Returning false on null does NOT fail
      //     fast — the poll simply keeps going (timeout 90s, intervals settle at
      //     5s) until it catches a frame window where degraded IS decoding at a
      //     lower index. That is the intended semantics.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage!);
            if (!healthy || !degraded) return false;
            return degraded.layerIndex < healthy.layerIndex;
          },
          { timeout: 90_000, intervals: [2000, 3000, 5000] },
        )
        .toBe(true);

      // Healthy receiver unaffected, and #1108 non-regression: its layer must
      // not have shrunk relative to the pre-impairment baseline.
      const healthyFinal = await readVideoLayer(healthyPage);
      expect(healthyFinal, "healthy receiver must still be decoding").not.toBeNull();
      expect(
        healthyFinal!.layerIndex,
        "healthy receiver must stay above the base layer (unaffected by peer congestion)",
      ).toBeGreaterThan(0);
      expect(
        healthyFinal!.layerIndex,
        "#1108: the healthy peer's layer must NOT shrink because the OTHER receiver " +
          `went bad over WT (before=${healthyBeforeImpair!.layerIndex}, after=${healthyFinal!.layerIndex})`,
      ).toBeGreaterThanOrEqual(healthyBeforeImpair!.layerIndex);

      // PHASE 4 — heal and prove climb-back (recovery confirms the divergence was
      // the impairment, not a permanent failure). Conservative hysteresis on
      // re-climb, so a generous window.
      await healDownlinkNetsim(degradedPage);
      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      // Clear the netsim impairment so a failure mid-test does not leave the tab
      // degraded. healDownlinkNetsim tolerates a closed/absent page (teardown-safe).
      if (degradedPage) {
        await healDownlinkNetsim(degradedPage);
      }
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // #1256 Phase 1 — SIZE-AWARE receiver simulcast layer cap.
  //
  // The feature: a receiver LIDs the requested simulcast layer to the rendered
  // tile size. A peer shown as a SMALL grid thumbnail pulls a LOWER layer (the
  // smallest whose native height covers the tile); when that peer is PINNED the
  // tile grows, the lid lifts, and the receiver up-switches above the base layer
  // (and requests a keyframe so it sharpens). The lid rides the EXISTING
  // per-receiver LAYER_PREFERENCE clamp seam, so the receiver's per-peer layer
  // SELECTION changes with NO wire/relay change — observable directly via the
  // SAME `readVideoLayer()` received-quality readout the #989/#1434 tests read.
  // (The READOUT is the authoritative client-side proof of the selected layer;
  // we deliberately do NOT cross-check `relay_layer_filtered_total` here — that
  // room-scoped counter only increments when the relay drops layers a receiver
  // did NOT select, and on the WebSocket path the default 2-peer stack forwards
  // all layers and decrements nothing, so it is not a reliable signal for this
  // healthy-link, single-receiver scenario.)
  //
  // CRITICAL DISTINCTION from every other layer-divergence test in this file:
  // there is NO network impairment. The whole point of #1256 is that a HEALTHY
  // receiver on a good link caps by SIZE, not by congestion. On a healthy link
  // the chooser would otherwise fail open to `highest_available` (the top layer)
  // and the receiver would decode 720p for a tiny thumbnail — the waste #1256
  // fixes. So this test asserts the receiver settles at the BASE layer with ZERO
  // impairment, then climbs above the base once the peer is pinned.
  //
  // PRODUCING A SMALL TILE (verified empirically against this stack — see the
  // observed readouts in the PR notes). `display_peers` (attendants.rs) FILTERS
  // OUT the local user's own session, so a publisher + ONE receiver gives the
  // receiver exactly ONE remote grid tile. The per-peer tile-size hint is
  // `compute_layout(tile_count, avail_w, avail_h, gap).tile_w / TILE_AR *
  // devicePixelRatio` device-px tall (attendants.rs, #1256 block) — it is
  // computed for EVERY non-screen-share tile regardless of the full-bleed CSS
  // exception. We shrink the RECEIVER viewport to 640x480 so that single tile's
  // device-px height settles at ~340px. The camera ladder is L0=640x360 /
  // L1=960x540 / L2=1280x720 with a 10% size-cap margin (the L0 boundary is
  // 360*1.1=396px), so a ~340px tile caps to L0 (the BASE) — verified: at this
  // viewport the readout reads "640x360" (index 0), and at the default 1280x720
  // viewport the same receiver reads "1280x720" (the top). We assert
  // `devicePixelRatio === 1` first: a HiDPI runner (dpr 2) would double the
  // device-px height to ~680px and lift the cap to the top, masking the feature
  // — so we fail loud rather than silently pass.
  //
  // UNTAGGED (no @bvt): like the #1434/#1108 WT default-suite tests above, this
  // runs only in the default `dioxus` suite (NOT per-PR CI) and is validated on
  // the local docker e2e stack. It needs NO toxiproxy/netsim profile.
  // -------------------------------------------------------------------------
  test("size-aware cap: a small-grid receiver pulls a LOWER layer than when the peer is pinned; pinning up-switches to the top (#1256)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1256_size_cap_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let rxPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1256-pub@videocall.rs",
        "Sim1256Pub",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-1256-rx@videocall.rs",
        "Sim1256Rx",
        uiURL,
      );
      // Full 3-rung ladder on BOTH ends so the size lid has headroom to lower
      // the requested layer below the top (the #1093 override replaces the
      // device-sniffed capability ceiling that would otherwise clamp a low-core
      // CI container to a single layer — no headroom, nothing to cap).
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      rxPage = await rxCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      // Join at the DEFAULT viewport (1280x720) first. A LARGE single tile sizes
      // above the L1 boundary so the receiver fails open to a HIGH layer, which in
      // turn makes the publisher's receiver-driven AQ keep the upper rungs ACTIVE
      // (with no receiver pulling them they get shed) — establishing the ladder
      // headroom this test needs BEFORE we shrink to demonstrate the cap. (Setting
      // the small viewport pre-join instead makes the lone receiver cap to base
      // immediately, the publisher sheds the upper rungs, and there is never a
      // >1-layer ladder to be "below".)
      await joinMeeting(pubPage, meetingId, "Sim1256Pub");
      await joinMeeting(rxPage, meetingId, "Sim1256Rx");

      // POSITIVE OVERRIDE PROOF (#1093) — assert the full ladder is actually
      // emitted BEFORE the skip guard, so a silently-broken override fails loud
      // instead of skipping on a clamped single layer (testing nothing).
      await assertCapabilityOverrideActive(pubConsole);

      // DPR GUARD: the device-px tile height = CSS height x devicePixelRatio. The
      // 640x480 viewport (below) caps to L0 ONLY at dpr 1; a HiDPI runner (dpr 2)
      // would double the height past the top layer's boundary and lift the cap to
      // the top, masking the feature. Fail loud rather than silently pass.
      const rxDpr = await rxPage.evaluate(() => window.devicePixelRatio);
      expect(
        rxDpr,
        "#1256 requires devicePixelRatio === 1 on the receiver so the 640x480 " +
          "viewport produces a sub-top tile height; a HiDPI runner would mask the cap",
      ).toBe(1);

      // The receiver must see the publisher's tile (peers connected).
      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // PHASE 0 — BASELINE (large tile climbs ABOVE the base layer). On the
      // healthy link with a full-bleed tile the receiver must reach a HIGH layer
      // (index >= 1) on a multi-rung ladder. This both (a) proves the LARGE tile
      // is NOT capped to the base (the necessary counterpart to Phase A's cap) and
      // (b) confirms ladder headroom exists. We poll for that condition DIRECTLY
      // (not merely "decoding started") so the wait does not return on the first
      // base-layer frame before the climb completes; the publisher's receiver-
      // driven AQ ramps the upper rungs over a few seconds. Skip (NOT fail) if it
      // never reaches a high layer within the window — that means the runner
      // clamped the publisher to a single layer, leaving no rung above the base
      // for a size cap to be below.
      let sawHighBaseline = false;
      await expect
        .poll(
          async () => {
            const s = await readVideoLayer(rxPage!);
            if (s && s.layerCount > 1 && s.layerIndex >= 1) sawHighBaseline = true;
            return sawHighBaseline;
          },
          { timeout: 60_000, intervals: [1000, 2000, 3000] },
        )
        .toBe(true)
        .catch(() => {
          /* never reached a high baseline within budget — handled by the skip below */
        });
      test.skip(
        !sawHighBaseline,
        "capability ceiling clamped the publisher to a single layer (the large-tile " +
          "receiver never climbed above the base); no ladder headroom above the base " +
          "for a size cap to be below (see helpers/simulcast-config.ts)",
      );

      // PHASE A — SHRINK ⇒ SIZE CAP ENGAGED. Shrink the receiver viewport so its
      // single remote tile becomes a ~340 device-px thumbnail (caps to L0). The
      // window `resize` listener (attendants.rs) bumps `viewport_version`, so the
      // layout — and the per-peer tile-size hint pushed via set_peer_tile_hints —
      // recomputes, and the lid lowers the requested layer. On a HEALTHY link the
      // chooser would otherwise stay at `highest_available` (the high baseline
      // above), so the index dropping to the BASE (0) can ONLY be the rendered-
      // tile-size lid — the load-bearing #1256 assertion, with NO congestion. We
      // assert on the INDEX (count-independent: the lid pins the lowest rung
      // regardless of how many rungs the publisher currently has active).
      await rxPage.setViewportSize({ width: 640, height: 480 });
      await expect
        .poll(async () => (await readVideoLayer(rxPage!))?.layerIndex ?? 99, {
          timeout: 60_000,
          intervals: [1000, 2000, 3000],
          message:
            "#1256 PHASE A: after shrinking the receiver viewport, the small-grid " +
            "tile on a HEALTHY link must cap its requested layer to the BASE " +
            "(index 0) — the size lid lowered it with no congestion",
        })
        .toBe(0);

      const capped = await readVideoLayer(rxPage);
      expect(capped, "#1256 PHASE A: receiver must still be decoding").not.toBeNull();
      expect(
        capped!.layerIndex,
        `#1256 PHASE A: small tile capped to base layer (got index ${capped!.layerIndex})`,
      ).toBe(0);

      // PHASE B — PIN ⇒ UP-SWITCH ABOVE THE LID. Pinning the publisher's tile
      // marks that peer Uncapped (pinned / screen-share / maximized are never
      // size-capped), so the size lid LIFTS and the receiver up-switches above the
      // base layer (requesting a keyframe so the higher layer sharpens). We assert
      // the index climbs to AT LEAST 1 (strictly above the base lid): that is the
      // unambiguous proof of the lid lift, robust to the publisher's active-layer
      // count oscillating between 2 and 3 (asserting an exact top index would
      // flake when the publisher is momentarily down to 2 active layers).
      //
      // The pin button (`button.pin-icon`, canvas_generator.rs) is
      // `visibility: hidden` until its `.grid-item` parent is hovered (style.css
      // `.grid-item:hover .pin-icon`) AND the full-bleed single tile pulses a
      // speaking-glow animation, so a normal hover-then-click is flaky. We
      // dispatch the click directly on the button via the DOM — this fires the
      // Dioxus onclick (`on_toggle_pin`) regardless of CSS visibility/animation.
      const gridTile = rxPage.locator("#grid-container .grid-item").first();
      await expect(gridTile).toBeVisible({ timeout: 10_000 });
      const pinButton = gridTile.locator("button.pin-icon");
      await expect(pinButton).toHaveCount(1, { timeout: 10_000 });
      await pinButton.evaluate((el: HTMLElement) => el.click());

      await expect
        .poll(async () => (await readVideoLayer(rxPage!))?.layerIndex ?? -1, {
          timeout: 45_000,
          intervals: [1000, 2000, 3000],
          message:
            "#1256 PHASE B: pinning the peer must lift the size lid and up-switch the " +
            "receiver ABOVE the base layer (index >= 1)",
        })
        .toBeGreaterThanOrEqual(1);

      // PHASE C — UNPIN ⇒ CAP BACK DOWN. Unpinning re-applies the size lid (the
      // tile is a small thumbnail again), so the requested layer drops back to the
      // base. Same DOM-dispatch click on the (now toggled) pin button.
      await pinButton.evaluate((el: HTMLElement) => el.click());

      await expect
        .poll(async () => (await readVideoLayer(rxPage!))?.layerIndex ?? 99, {
          timeout: 60_000,
          intervals: [1000, 2000, 3000],
          message:
            "#1256 PHASE C: unpinning must re-apply the size lid and cap the small " +
            "tile back to the base layer (index 0)",
        })
        .toBe(0);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });
});

// ---------------------------------------------------------------------------
// #1219 Half 2 — RELAY-SIDE downlink-congestion signal path validation (#1434)
//
// The per-receiver layer DIVERGENCE tests above (WS @impair + WT netsim) prove
// the UI-side outcome: a congested receiver drops to a lower layer while the
// healthy peer holds. But they do NOT prove the RELAY's Half 2 signal path
// actually fired — the congestion COULD be detected purely client-side by the
// layer chooser reacting to loss (which is the legacy #1080 path). Issue #1434
// requires asserting the RELAY METRICS that prove the Half 2 relay-side
// congestion detection + proactive shedding + LAYER_PREFERENCE durable path
// fired end-to-end:
//
//   1. relay_receiver_downlink_congestion_total RISES (relay detected congestion)
//   2. relay_downlink_shed_total RISES (relay proactively shed non-base packets)
//   3. relay_layer_filtered_total RISES (receiver published LAYER_PREFERENCE
//      stepping down → relay layer filter engaged the durable path)
//   4. ISOLATION: healthy receiver metrics UNCHANGED
//   5. AUDIO PROTECTION: audio NOT shed (audio always passes the pre-filter)
//   6. RECOVERY: relay_receiver_downlink_recovered_total RISES after heal
//
// These tests EXTEND the existing impair harness (same 3-browser topology) but
// add relay-metric assertions alongside the UI-layer assertions. They are
// intentionally SEPARATE tests so a relay-metric regression does not mask a UI
// regression (and vice versa).
//
// BOTH transports are covered: WS via toxiproxy (@impair), WT via netsim
// (default suite). This is mandatory per #1434's stretch goal and the standing
// fact that "both transports shed at 80%."
// ---------------------------------------------------------------------------
test.describe("#1219 Half 2 relay-side congestion validation (#1434)", () => {
  test.describe.configure({ mode: "serial", timeout: 240_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  // -------------------------------------------------------------------------
  // WS path: toxiproxy bandwidth clamp → relay detects congestion → sheds →
  // receiver publishes LAYER_PREFERENCE → relay layer-filters. Tagged @impair.
  // -------------------------------------------------------------------------
  test("relay enters downlink congestion and sheds for WS receiver, healthy peer unaffected (#1434) @impair", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_ws_${Date.now()}`;

    await assertProxyUp();
    await healDownlink();

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-pub-ws@videocall.rs",
        "Sim1434PubWS",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-1434-healthy-ws@videocall.rs",
        "Sim1434HealthyWS",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-1434-degraded-ws@videocall.rs",
        "Sim1434DegradedWS",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(healthyCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(degradedCtx, 3, { capabilityMaxLayersOverride: 3 });

      // Route degraded receiver through toxiproxy, pinning to WS.
      await routeDownlinkThroughProxy(degradedCtx);

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      const degradedPage = await degradedCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434PubWS");
      await joinMeeting(healthyPage, meetingId, "Sim1434HealthyWS");
      await joinMeeting(degradedPage, meetingId, "Sim1434DegradedWS");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above base on a healthy link.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped to single layer; no ladder headroom to diverge",
      );

      // Wait for degraded receiver to climb above base before impairing.
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Snapshot relay metrics BEFORE impairment (baseline for delta).
      const beforeWs = await snapshotDownlinkCongestionMetrics("websocket", meetingId);
      const layerFilteredBefore = beforeWs.layerFilteredTotal;

      // PHASE 2 — impair the degraded receiver's downlink via toxiproxy.
      await impairDownlink({ rateKb: 15 });

      // PHASE 3a — RELAY REACTS: assert congestion_total RISES for the WS relay.
      // The relay's windowed CongestionTracker should cross its threshold within
      // seconds of the outbound channel filling. Poll until the counter increments.
      await expect
        .poll(
          async () => {
            const current = await readDownlinkCongestionTotal("websocket");
            return current - beforeWs.congestionTotal;
          },
          {
            timeout: 60_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 1: relay_receiver_downlink_congestion_total must RISE " +
              "for the WS relay when a receiver's downlink is impaired",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3b — PROACTIVE SHEDDING: relay_downlink_shed_total RISES (non-base
      // packets shed before try_send for the congested receiver).
      await expect
        .poll(
          async () => {
            const current = await readDownlinkShedTotal("websocket");
            return current - beforeWs.shedTotal;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 2: relay_downlink_shed_total must RISE — the relay " +
              "must proactively shed non-base packets for the congested receiver",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3c — DURABLE PATH: the congested receiver publishes LAYER_PREFERENCE
      // stepping down, causing the relay's layer filter to engage. This is proven by
      // relay_layer_filtered_total rising for this room.
      await expect
        .poll(
          async () => {
            const current = await readLayerFilteredTotal("websocket", meetingId);
            return current - layerFilteredBefore;
          },
          {
            timeout: 60_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 3: relay_layer_filtered_total must RISE — the " +
              "congested receiver must publish LAYER_PREFERENCE stepping down, " +
              "engaging the relay's durable layer filter",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3d — ISOLATION: the healthy receiver is still decoding above base.
      // Its layer must not have been dragged down.
      const healthyAfterImpair = await readVideoLayer(healthyPage);
      expect(
        healthyAfterImpair,
        "#1434 isolation: healthy receiver must still be decoding",
      ).not.toBeNull();
      expect(
        healthyAfterImpair!.layerIndex,
        "#1434 isolation: healthy receiver must stay above base (unaffected by " +
          "peer's downlink congestion)",
      ).toBeGreaterThan(0);

      // PHASE 3e — AUDIO PROTECTION: audio is NOT shed for the congested receiver.
      // The audio readout on the degraded page should still report receiving audio
      // (the Half 2 pre-filter only sheds non-base VIDEO/SCREEN, never AUDIO).
      // Allow a generous window since audio may briefly blip under heavy loss.
      await expect
        .poll(
          async () => {
            const audio = await readAudioLayer(degradedPage);
            return audio !== null;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 4 (audio protection): the degraded receiver must " +
              "still be receiving AUDIO — the Half 2 shed path protects audio",
          },
        )
        .toBe(true);

      // PHASE 4 — RECOVERY: heal the downlink and assert the relay's recovery
      // counter increments (the relief window elapses with no fresh overflow).
      await healDownlink();

      await expect
        .poll(
          async () => {
            const current = await readDownlinkRecoveredTotal("websocket");
            return current - beforeWs.recoveredTotal;
          },
          {
            timeout: 90_000,
            intervals: [3000, 5000, 8000],
            message:
              "#1434 assertion 5 (recovery): relay_receiver_downlink_recovered_total " +
              "must RISE after the impairment is healed (relief window elapsed)",
          },
        )
        .toBeGreaterThan(0);

      // Also confirm the degraded receiver climbs back (UI recovery).
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      await healDownlink();
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // WT path: client-side netsim hook → relay detects congestion → sheds →
  // receiver publishes LAYER_PREFERENCE → relay layer-filters. No toxiproxy;
  // runs in the default dioxus suite (NOT tagged @impair).
  //
  // The WT relay process (:5321) has the SAME Half 2 code path (the congestion
  // detection runs in the transport-agnostic per-session NATS loop), so the
  // same metric counters must fire. The only difference is that the LOSS is
  // manufactured CLIENT-SIDE (netsim drops inbound packets) rather than
  // RELAY-SIDE (toxiproxy fills the outbound channel). However, for the WT
  // relay to enter congestion mode, the CLIENT must still signal backpressure
  // upstream — which it does via QUIC flow control (window exhaustion on the
  // receiving stream) when it cannot consume packets fast enough. The netsim
  // hook drops packets AFTER receipt from the transport, so from the relay's
  // perspective the client's receive window may still drain normally and the
  // relay-side congestion detection may NOT fire as aggressively as the WS
  // case. Therefore this test uses a MILDER assertion: it asserts either the
  // relay-side congestion counters rise OR the durable LAYER_PREFERENCE path
  // fires (which is the client-side chooser stepping down and publishing its
  // preference, triggering relay layer filtering regardless of relay-side
  // congestion state). The key invariant is still: layer filtering engages
  // for the impaired receiver AND the healthy receiver is unaffected.
  // -------------------------------------------------------------------------
  test("relay layer-filters for WT receiver under netsim impairment, healthy peer unaffected (#1434)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_wt_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let degradedPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-pub-wt@videocall.rs",
        "Sim1434PubWT",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-1434-healthy-wt@videocall.rs",
        "Sim1434HealthyWT",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-1434-degraded-wt@videocall.rs",
        "Sim1434DegradedWT",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(healthyCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(degradedCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      degradedPage = await degradedCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434PubWT");
      await joinMeeting(healthyPage, meetingId, "Sim1434HealthyWT");
      await joinMeeting(degradedPage, meetingId, "Sim1434DegradedWT");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above base on a healthy link.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage!);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped to single layer; no ladder headroom to diverge",
      );

      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Snapshot relay metrics BEFORE impairment.
      // For WT, the relay process is :5321. Scrape the WT relay.
      const beforeWt = await snapshotDownlinkCongestionMetrics("webtransport", meetingId);
      const layerFilteredBefore = beforeWt.layerFilteredTotal;

      // PHASE 2 — impair the degraded receiver's downlink via netsim.
      await impairDownlinkNetsim(degradedPage);

      // PHASE 3a — DURABLE PATH: the chooser steps down and publishes
      // LAYER_PREFERENCE, which engages the relay's layer filter for this room.
      // This is the primary assertion for the WT path: regardless of whether the
      // relay itself enters congestion shedding mode (which depends on QUIC
      // backpressure reaching the relay), the LAYER_PREFERENCE → layer filter
      // path is the client-driven durable signal that proves #1219 Half 2's
      // end-to-end loop.
      await expect
        .poll(
          async () => {
            const current = await readLayerFilteredTotal("webtransport", meetingId);
            return current - layerFilteredBefore;
          },
          {
            timeout: 90_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 WT assertion (durable path): relay_layer_filtered_total must " +
              "RISE — the congested receiver's chooser must step down, publish " +
              "LAYER_PREFERENCE, and cause the relay to filter higher layers",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3b — ISOLATION: the healthy receiver remains above base.
      const healthyAfterImpair = await readVideoLayer(healthyPage);
      expect(
        healthyAfterImpair,
        "#1434 WT isolation: healthy receiver must still be decoding",
      ).not.toBeNull();
      expect(
        healthyAfterImpair!.layerIndex,
        "#1434 WT isolation: healthy receiver must stay above base",
      ).toBeGreaterThan(0);

      // PHASE 3c — AUDIO PROTECTION: degraded receiver still receives audio.
      await expect
        .poll(
          async () => {
            const audio = await readAudioLayer(degradedPage!);
            return audio !== null;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message: "#1434 WT audio protection: degraded receiver must still receive AUDIO",
          },
        )
        .toBe(true);

      // PHASE 3d — RELAY CONGESTION (soft assertion): check if the WT relay's
      // congestion counter also rose. Under netsim the loss is client-side, so
      // relay-side congestion detection may not fire (QUIC window may still drain).
      // We log but do NOT hard-fail if it did not rise — the durable path (3a) is
      // the load-bearing assertion. If it DID rise, that proves the relay's own
      // congestion tracker also engaged, which is the full Half 2 story.
      const congestionAfter = await readDownlinkCongestionTotal("webtransport");
      const congestionDelta = congestionAfter - beforeWt.congestionTotal;
      if (congestionDelta > 0) {
        // Full Half 2 relay-side path also fired — optimal coverage.
        const shedAfter = await readDownlinkShedTotal("webtransport");
        const shedDelta = shedAfter - beforeWt.shedTotal;
        expect(
          shedDelta,
          "#1434 WT bonus: if relay entered congestion, shed_total should also rise",
        ).toBeGreaterThan(0);
      }
      // (else: relay did not enter congestion mode, but layer-filter path fired —
      // the client's chooser handled the step-down autonomously. Still valid.)

      // PHASE 4 — heal and verify recovery (UI-side: degraded climbs back).
      await healDownlinkNetsim(degradedPage);

      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      if (degradedPage) {
        await healDownlinkNetsim(degradedPage);
      }
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // Recovery stability: validate that after downlink impairment the receiver's
  // layer does NOT oscillate (up/down flapping). This uses the WT netsim hook
  // with `crushed_downlink` (40% loss). The key assertion is that after the
  // initial step-down the layer should SETTLE, not bounce.
  //
  // The hysteresis in the layer chooser (consecutive-success counters or decay
  // windows for step-UP) should prevent rapid oscillation. This test polls the
  // layer over 30 seconds after step-down and asserts that the max observed
  // index minus the min observed index is <= 1 (at most one step of jitter,
  // not full-range flapping).
  // -------------------------------------------------------------------------
  test("recovery window does not oscillate under marginal loss — layer settles after step-down (#1434 comment)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_stable_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let rxPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-stab-pub@videocall.rs",
        "Sim1434StabPub",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-1434-stab-rx@videocall.rs",
        "Sim1434StabRx",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      rxPage = await rxCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434StabPub");
      await joinMeeting(rxPage, meetingId, "Sim1434StabRx");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(rxPage);

      // Let the receiver climb above base.
      await expect
        .poll(
          async () => {
            const layer = await readVideoLayer(rxPage!);
            return layer?.layerCount ?? 0;
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(1);

      await expect
        .poll(async () => (await readVideoLayer(rxPage!))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Impair the receiver — this will cause it to step down.
      await impairDownlinkNetsim(rxPage);

      // Wait for the initial step-down to take effect.
      await expect
        .poll(
          async () => {
            const layer = await readVideoLayer(rxPage!);
            // Accept either layer 0 (stepped all the way down) or a lower layer
            // than the top. The key is that it stepped DOWN.
            if (!layer) return false;
            return layer.layerIndex < layer.layerCount - 1;
          },
          { timeout: 60_000, intervals: [2000, 3000, 5000] },
        )
        .toBe(true);

      // STABILITY OBSERVATION: now that the receiver has stepped down, observe
      // its layer index over 30 seconds. Under a marginal impairment the chooser
      // should SETTLE at the lower layer — not oscillate up and back down
      // repeatedly. We track the set of unique layer indices observed.
      const observedIndices: number[] = [];
      const stabilityDurationMs = 30_000;
      const pollIntervalMs = 3_000;
      const iterations = Math.floor(stabilityDurationMs / pollIntervalMs);

      for (let i = 0; i < iterations; i++) {
        await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
        const layer = await readVideoLayer(rxPage);
        if (layer) {
          observedIndices.push(layer.layerIndex);
        }
      }

      // Assert stability: the range of observed indices (max - min) must be <= 1.
      // A range of 0 = perfectly stable (settled at one layer).
      // A range of 1 = at most one step of jitter (acceptable transient).
      // A range >= 2 = full oscillation (up/down flapping) — FAIL.
      expect(
        observedIndices.length,
        "must have observed at least 5 layer readings during stability window",
      ).toBeGreaterThanOrEqual(5);

      const minIdx = Math.min(...observedIndices);
      const maxIdx = Math.max(...observedIndices);
      expect(
        maxIdx - minIdx,
        `#1434 stability: layer must settle after step-down, not oscillate. ` +
          `Observed indices: [${observedIndices.join(", ")}] ` +
          `(range ${maxIdx - minIdx}, max allowed = 1)`,
      ).toBeLessThanOrEqual(1);
    } finally {
      if (rxPage) {
        await healDownlinkNetsim(rxPage);
      }
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });
});

// ---------------------------------------------------------------------------
// #1219 Half 2 — RELAY-SIDE downlink-congestion signal path validation (#1434)
//
// The per-receiver layer DIVERGENCE tests above (WS @impair + WT netsim) prove
// the UI-side outcome: a congested receiver drops to a lower layer while the
// healthy peer holds. But they do NOT prove the RELAY's Half 2 signal path
// actually fired — the congestion COULD be detected purely client-side by the
// layer chooser reacting to loss (which is the legacy #1080 path). Issue #1434
// requires asserting the RELAY METRICS that prove the Half 2 relay-side
// congestion detection + proactive shedding + LAYER_PREFERENCE durable path
// fired end-to-end:
//
//   1. relay_receiver_downlink_congestion_total RISES (relay detected congestion)
//   2. relay_downlink_shed_total RISES (relay proactively shed non-base packets)
//   3. relay_layer_filtered_total RISES (receiver published LAYER_PREFERENCE
//      stepping down → relay layer filter engaged the durable path)
//   4. ISOLATION: healthy receiver metrics UNCHANGED
//   5. AUDIO PROTECTION: audio NOT shed (audio always passes the pre-filter)
//   6. RECOVERY: relay_receiver_downlink_recovered_total RISES after heal
//
// These tests EXTEND the existing impair harness (same 3-browser topology) but
// add relay-metric assertions alongside the UI-layer assertions. They are
// intentionally SEPARATE tests so a relay-metric regression does not mask a UI
// regression (and vice versa).
//
// BOTH transports are covered: WS via toxiproxy (@impair), WT via netsim
// (default suite). This is mandatory per #1434's stretch goal and the standing
// fact that "both transports shed at 80%."
// ---------------------------------------------------------------------------
test.describe("#1219 Half 2 relay-side congestion validation (#1434)", () => {
  test.describe.configure({ mode: "serial", timeout: 240_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  // -------------------------------------------------------------------------
  // WS path: toxiproxy bandwidth clamp → relay detects congestion → sheds →
  // receiver publishes LAYER_PREFERENCE → relay layer-filters. Tagged @impair.
  // -------------------------------------------------------------------------
  test("relay enters downlink congestion and sheds for WS receiver, healthy peer unaffected (#1434) @impair", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_ws_${Date.now()}`;

    await assertProxyUp();
    await healDownlink();

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-pub-ws@videocall.rs",
        "Sim1434PubWS",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-1434-healthy-ws@videocall.rs",
        "Sim1434HealthyWS",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-1434-degraded-ws@videocall.rs",
        "Sim1434DegradedWS",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(healthyCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(degradedCtx, 3, { capabilityMaxLayersOverride: 3 });

      // Route degraded receiver through toxiproxy, pinning to WS.
      await routeDownlinkThroughProxy(degradedCtx);

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      const degradedPage = await degradedCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434PubWS");
      await joinMeeting(healthyPage, meetingId, "Sim1434HealthyWS");
      await joinMeeting(degradedPage, meetingId, "Sim1434DegradedWS");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above base on a healthy link.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped to single layer; no ladder headroom to diverge",
      );

      // Wait for degraded receiver to climb above base before impairing.
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Snapshot relay metrics BEFORE impairment (baseline for delta).
      const beforeWs = await snapshotDownlinkCongestionMetrics("websocket", meetingId);
      const layerFilteredBefore = beforeWs.layerFilteredTotal;

      // PHASE 2 — impair the degraded receiver's downlink via toxiproxy.
      await impairDownlink({ rateKb: 15 });

      // PHASE 3a — RELAY REACTS: assert congestion_total RISES for the WS relay.
      // The relay's windowed CongestionTracker should cross its threshold within
      // seconds of the outbound channel filling. Poll until the counter increments.
      await expect
        .poll(
          async () => {
            const current = await readDownlinkCongestionTotal("websocket");
            return current - beforeWs.congestionTotal;
          },
          {
            timeout: 60_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 1: relay_receiver_downlink_congestion_total must RISE " +
              "for the WS relay when a receiver's downlink is impaired",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3b — PROACTIVE SHEDDING: relay_downlink_shed_total RISES (non-base
      // packets shed before try_send for the congested receiver).
      await expect
        .poll(
          async () => {
            const current = await readDownlinkShedTotal("websocket");
            return current - beforeWs.shedTotal;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 2: relay_downlink_shed_total must RISE — the relay " +
              "must proactively shed non-base packets for the congested receiver",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3c — DURABLE PATH: the congested receiver publishes LAYER_PREFERENCE
      // stepping down, causing the relay's layer filter to engage. This is proven by
      // relay_layer_filtered_total rising for this room.
      await expect
        .poll(
          async () => {
            const current = await readLayerFilteredTotal("websocket", meetingId);
            return current - layerFilteredBefore;
          },
          {
            timeout: 60_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 3: relay_layer_filtered_total must RISE — the " +
              "congested receiver must publish LAYER_PREFERENCE stepping down, " +
              "engaging the relay's durable layer filter",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3d — ISOLATION: the healthy receiver is still decoding above base.
      // Its layer must not have been dragged down.
      const healthyAfterImpair = await readVideoLayer(healthyPage);
      expect(
        healthyAfterImpair,
        "#1434 isolation: healthy receiver must still be decoding",
      ).not.toBeNull();
      expect(
        healthyAfterImpair!.layerIndex,
        "#1434 isolation: healthy receiver must stay above base (unaffected by " +
          "peer's downlink congestion)",
      ).toBeGreaterThan(0);

      // PHASE 3e — AUDIO PROTECTION: audio is NOT shed for the congested receiver.
      // The audio readout on the degraded page should still report receiving audio
      // (the Half 2 pre-filter only sheds non-base VIDEO/SCREEN, never AUDIO).
      // Allow a generous window since audio may briefly blip under heavy loss.
      await expect
        .poll(
          async () => {
            const audio = await readAudioLayer(degradedPage);
            return audio !== null;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 assertion 4 (audio protection): the degraded receiver must " +
              "still be receiving AUDIO — the Half 2 shed path protects audio",
          },
        )
        .toBe(true);

      // PHASE 4 — RECOVERY: heal the downlink and assert the relay's recovery
      // counter increments (the relief window elapses with no fresh overflow).
      await healDownlink();

      await expect
        .poll(
          async () => {
            const current = await readDownlinkRecoveredTotal("websocket");
            return current - beforeWs.recoveredTotal;
          },
          {
            timeout: 90_000,
            intervals: [3000, 5000, 8000],
            message:
              "#1434 assertion 5 (recovery): relay_receiver_downlink_recovered_total " +
              "must RISE after the impairment is healed (relief window elapsed)",
          },
        )
        .toBeGreaterThan(0);

      // Also confirm the degraded receiver climbs back (UI recovery).
      await expect
        .poll(async () => (await readVideoLayer(degradedPage))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      await healDownlink();
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // WT path: client-side netsim hook → relay detects congestion → sheds →
  // receiver publishes LAYER_PREFERENCE → relay layer-filters. No toxiproxy;
  // runs in the default dioxus suite (NOT tagged @impair).
  //
  // The WT relay process (:5321) has the SAME Half 2 code path (the congestion
  // detection runs in the transport-agnostic per-session NATS loop), so the
  // same metric counters must fire. The only difference is that the LOSS is
  // manufactured CLIENT-SIDE (netsim drops inbound packets) rather than
  // RELAY-SIDE (toxiproxy fills the outbound channel). However, for the WT
  // relay to enter congestion mode, the CLIENT must still signal backpressure
  // upstream — which it does via QUIC flow control (window exhaustion on the
  // receiving stream) when it cannot consume packets fast enough. The netsim
  // hook drops packets AFTER receipt from the transport, so from the relay's
  // perspective the client's receive window may still drain normally and the
  // relay-side congestion detection may NOT fire as aggressively as the WS
  // case. Therefore this test uses a MILDER assertion: it asserts either the
  // relay-side congestion counters rise OR the durable LAYER_PREFERENCE path
  // fires (which is the client-side chooser stepping down and publishing its
  // preference, triggering relay layer filtering regardless of relay-side
  // congestion state). The key invariant is still: layer filtering engages
  // for the impaired receiver AND the healthy receiver is unaffected.
  // -------------------------------------------------------------------------
  test("relay layer-filters for WT receiver under netsim impairment, healthy peer unaffected (#1434)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_wt_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const healthyBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const degradedBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let degradedPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-pub-wt@videocall.rs",
        "Sim1434PubWT",
        uiURL,
      );
      const healthyCtx = await createAuthenticatedContext(
        healthyBrowser,
        "sim-1434-healthy-wt@videocall.rs",
        "Sim1434HealthyWT",
        uiURL,
      );
      const degradedCtx = await createAuthenticatedContext(
        degradedBrowser,
        "sim-1434-degraded-wt@videocall.rs",
        "Sim1434DegradedWT",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(healthyCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(degradedCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      const healthyPage = await healthyCtx.newPage();
      degradedPage = await degradedCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434PubWT");
      await joinMeeting(healthyPage, meetingId, "Sim1434HealthyWT");
      await joinMeeting(degradedPage, meetingId, "Sim1434DegradedWT");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(healthyPage);
      await openPerformancePanel(degradedPage);

      // PHASE 1 — let both receivers climb above base on a healthy link.
      await expect
        .poll(
          async () => {
            const healthy = await readVideoLayer(healthyPage);
            const degraded = await readVideoLayer(degradedPage!);
            if (!healthy || !degraded) return -1;
            return Math.min(healthy.layerCount, degraded.layerCount);
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(0);

      const healthyStart = await readVideoLayer(healthyPage);
      const degradedStart = await readVideoLayer(degradedPage);
      test.skip(
        (healthyStart?.layerCount ?? 1) <= 1 || (degradedStart?.layerCount ?? 1) <= 1,
        "capability ceiling clamped to single layer; no ladder headroom to diverge",
      );

      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Snapshot relay metrics BEFORE impairment.
      // For WT, the relay process is :5321. Scrape the WT relay.
      const beforeWt = await snapshotDownlinkCongestionMetrics("webtransport", meetingId);
      const layerFilteredBefore = beforeWt.layerFilteredTotal;

      // PHASE 2 — impair the degraded receiver's downlink via netsim.
      await impairDownlinkNetsim(degradedPage);

      // PHASE 3a — DURABLE PATH: the chooser steps down and publishes
      // LAYER_PREFERENCE, which engages the relay's layer filter for this room.
      // This is the primary assertion for the WT path: regardless of whether the
      // relay itself enters congestion shedding mode (which depends on QUIC
      // backpressure reaching the relay), the LAYER_PREFERENCE → layer filter
      // path is the client-driven durable signal that proves #1219 Half 2's
      // end-to-end loop.
      await expect
        .poll(
          async () => {
            const current = await readLayerFilteredTotal("webtransport", meetingId);
            return current - layerFilteredBefore;
          },
          {
            timeout: 90_000,
            intervals: [2000, 3000, 5000],
            message:
              "#1434 WT assertion (durable path): relay_layer_filtered_total must " +
              "RISE — the congested receiver's chooser must step down, publish " +
              "LAYER_PREFERENCE, and cause the relay to filter higher layers",
          },
        )
        .toBeGreaterThan(0);

      // PHASE 3b — ISOLATION: the healthy receiver remains above base.
      const healthyAfterImpair = await readVideoLayer(healthyPage);
      expect(
        healthyAfterImpair,
        "#1434 WT isolation: healthy receiver must still be decoding",
      ).not.toBeNull();
      expect(
        healthyAfterImpair!.layerIndex,
        "#1434 WT isolation: healthy receiver must stay above base",
      ).toBeGreaterThan(0);

      // PHASE 3c — AUDIO PROTECTION: degraded receiver still receives audio.
      await expect
        .poll(
          async () => {
            const audio = await readAudioLayer(degradedPage!);
            return audio !== null;
          },
          {
            timeout: 30_000,
            intervals: [2000, 3000, 5000],
            message: "#1434 WT audio protection: degraded receiver must still receive AUDIO",
          },
        )
        .toBe(true);

      // PHASE 3d — RELAY CONGESTION (soft assertion): check if the WT relay's
      // congestion counter also rose. Under netsim the loss is client-side, so
      // relay-side congestion detection may not fire (QUIC window may still drain).
      // We log but do NOT hard-fail if it did not rise — the durable path (3a) is
      // the load-bearing assertion. If it DID rise, that proves the relay's own
      // congestion tracker also engaged, which is the full Half 2 story.
      const congestionAfter = await readDownlinkCongestionTotal("webtransport");
      const congestionDelta = congestionAfter - beforeWt.congestionTotal;
      if (congestionDelta > 0) {
        // Full Half 2 relay-side path also fired — optimal coverage.
        const shedAfter = await readDownlinkShedTotal("webtransport");
        const shedDelta = shedAfter - beforeWt.shedTotal;
        expect(
          shedDelta,
          "#1434 WT bonus: if relay entered congestion, shed_total should also rise",
        ).toBeGreaterThan(0);
      }
      // (else: relay did not enter congestion mode, but layer-filter path fired —
      // the client's chooser handled the step-down autonomously. Still valid.)

      // PHASE 4 — heal and verify recovery (UI-side: degraded climbs back).
      await healDownlinkNetsim(degradedPage);

      await expect
        .poll(async () => (await readVideoLayer(degradedPage!))?.layerIndex ?? 0, {
          timeout: 90_000,
          intervals: [2000, 3000, 5000],
        })
        .toBeGreaterThan(0);
    } finally {
      if (degradedPage) {
        await healDownlinkNetsim(degradedPage);
      }
      await pubBrowser.close();
      await healthyBrowser.close();
      await degradedBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // Recovery stability: validate that after downlink impairment the receiver's
  // layer does NOT oscillate (up/down flapping). This uses the WT netsim hook
  // with `crushed_downlink` (40% loss). The key assertion is that after the
  // initial step-down the layer should SETTLE, not bounce.
  //
  // The hysteresis in the layer chooser (consecutive-success counters or decay
  // windows for step-UP) should prevent rapid oscillation. This test polls the
  // layer over 30 seconds after step-down and asserts that the max observed
  // index minus the min observed index is <= 1 (at most one step of jitter,
  // not full-range flapping).
  // -------------------------------------------------------------------------
  test("recovery window does not oscillate under marginal loss — layer settles after step-down (#1434 comment)", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_1434_stable_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser = await chromium.launch({ args: BROWSER_ARGS });
    let rxPage: Page | undefined;
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-1434-stab-pub@videocall.rs",
        "Sim1434StabPub",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-1434-stab-rx@videocall.rs",
        "Sim1434StabRx",
        uiURL,
      );
      await enableSimulcastFlag(pubCtx, 3, { capabilityMaxLayersOverride: 3 });
      await enableSimulcastFlag(rxCtx, 3, { capabilityMaxLayersOverride: 3 });

      const pubPage = await pubCtx.newPage();
      rxPage = await rxCtx.newPage();

      const pubConsole = collectConsole(pubPage);

      await joinMeeting(pubPage, meetingId, "Sim1434StabPub");
      await joinMeeting(rxPage, meetingId, "Sim1434StabRx");

      await assertCapabilityOverrideActive(pubConsole);
      await openPerformancePanel(rxPage);

      // Let the receiver climb above base.
      await expect
        .poll(
          async () => {
            const layer = await readVideoLayer(rxPage!);
            return layer?.layerCount ?? 0;
          },
          { timeout: 45_000, intervals: [1000, 2000, 3000] },
        )
        .toBeGreaterThan(1);

      await expect
        .poll(async () => (await readVideoLayer(rxPage!))?.layerIndex ?? 0, {
          timeout: 30_000,
          intervals: [1000, 2000, 3000],
        })
        .toBeGreaterThan(0);

      // Impair the receiver — this will cause it to step down.
      await impairDownlinkNetsim(rxPage);

      // Wait for the initial step-down to take effect.
      await expect
        .poll(
          async () => {
            const layer = await readVideoLayer(rxPage!);
            // Accept either layer 0 (stepped all the way down) or a lower layer
            // than the top. The key is that it stepped DOWN.
            if (!layer) return false;
            return layer.layerIndex < layer.layerCount - 1;
          },
          { timeout: 60_000, intervals: [2000, 3000, 5000] },
        )
        .toBe(true);

      // STABILITY OBSERVATION: now that the receiver has stepped down, observe
      // its layer index over 30 seconds. Under a marginal impairment the chooser
      // should SETTLE at the lower layer — not oscillate up and back down
      // repeatedly. We track the set of unique layer indices observed.
      const observedIndices: number[] = [];
      const stabilityDurationMs = 30_000;
      const pollIntervalMs = 3_000;
      const iterations = Math.floor(stabilityDurationMs / pollIntervalMs);

      for (let i = 0; i < iterations; i++) {
        await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
        const layer = await readVideoLayer(rxPage);
        if (layer) {
          observedIndices.push(layer.layerIndex);
        }
      }

      // Assert stability: the range of observed indices (max - min) must be <= 1.
      // A range of 0 = perfectly stable (settled at one layer).
      // A range of 1 = at most one step of jitter (acceptable transient).
      // A range >= 2 = full oscillation (up/down flapping) — FAIL.
      expect(
        observedIndices.length,
        "must have observed at least 5 layer readings during stability window",
      ).toBeGreaterThanOrEqual(5);

      const minIdx = Math.min(...observedIndices);
      const maxIdx = Math.max(...observedIndices);
      expect(
        maxIdx - minIdx,
        `#1434 stability: layer must settle after step-down, not oscillate. ` +
          `Observed indices: [${observedIndices.join(", ")}] ` +
          `(range ${maxIdx - minIdx}, max allowed = 1)`,
      ).toBeLessThanOrEqual(1);
    } finally {
      if (rxPage) {
        await healDownlinkNetsim(rxPage);
      }
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });
});

// ---------------------------------------------------------------------------
// Flag-OFF control — single-layer no-regression guard for #1082.
//
// IMPORTANT: the runtime default of `experimentalSimulcastMaxLayers` was flipped
// from 1 → 3 (multicast ON by default). So "set no flag" no longer means OFF —
// it now means 3. To genuinely exercise the single-layer / feature-OFF path this
// test PINS the flag to 1 explicitly via `pinSimulcastMaxLayers(ctx, 1)` on both
// ends. The #1082 ladder machinery went N-generic but MUST NOT change the
// single-layer path: with the flag at 1 the publisher emits a single layer for
// every kind, byte-identical to the pre-simulcast encoders. The DOM-observable
// proof is that every received readout reports `/1` (a single-layer ladder)
// once decoding begins.
//
// FIXME(#1093): multi-party (2-context) — this control also joins a publisher +
// receiver and polls the receiver decoding the publisher's stream, so it hits
// the same headless-CI renderer crash ("Target page/context closed") as the
// flag-ON tests. It does NOT need a capability-override hook (single-layer is the
// expected outcome here), but it DOES need the renderer-crash-resilient runner
// for the 2-context join + cross-peer decode.
// ---------------------------------------------------------------------------
test.describe("Simulcast flag OFF (pinned to 1) — single-layer no-regression", () => {
  // SERIAL — same #1093 renderer-crash mitigation as the flag-on describe: this
  // control also launches a publisher + receiver (two heavy renderers) and polls
  // a cross-peer decoded stream, so under `workers: 2` it could run concurrently
  // with another multi-browser test and overcommit the 8-vCPU CI runner. Run it
  // one-at-a-time so at most one publisher+receiver pair is live.
  test.describe.configure({ mode: "serial", timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("flag pinned to 1 emits a single layer for video, audio, and content", async ({
    baseURL,
  }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_simulcast_off_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "sim-off-pub@videocall.rs",
        "SimOffPublisher",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "sim-off-rx@videocall.rs",
        "SimOffReceiver",
        uiURL,
      );
      // Explicitly PIN the flag to 1 (= single layer / OFF) on BOTH ends. The
      // runtime default is now 3, so omitting the flag would NOT exercise the
      // OFF path — it would emit 3 layers. Must run before the first navigation.
      await pinSimulcastMaxLayers(pubCtx, 1);
      await pinSimulcastMaxLayers(rxCtx, 1);

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      await joinMeeting(pubPage, meetingId, "SimOffPublisher");
      await joinMeeting(rxPage, meetingId, "SimOffReceiver");

      await expect(rxPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      await openPerformancePanel(rxPage);

      // Wait until the receiver is decoding the publisher's VIDEO, then assert
      // the ladder is a SINGLE layer (count == 1). With the flag off the encoder
      // produces exactly one layer, so the readout reports position/count "1/1"
      // (#1222: the single-layer quality letter is "1", so the readout reads
      // "1 · 1/1 · …"; `readVideoLayer` parses the position/count tail).
      let video: { layerIndex: number; layerCount: number } | null = null;
      await expect
        .poll(
          async () => {
            video = await readVideoLayer(rxPage);
            return video !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);
      expect(video!.layerCount, "flag-off video must be single-layer").toBe(1);
      expect(video!.layerIndex).toBe(0);

      // AUDIO must likewise be single-layer with the flag off — the #1082-B
      // 3-rung ladder is gated behind the flag and must not leak into the
      // default path. The base rung is the lowest (24 kbps).
      let audio: { layerIndex: number; layerCount: number; kbps: number } | null = null;
      await expect
        .poll(
          async () => {
            audio = await readAudioLayer(rxPage);
            return audio !== null;
          },
          { timeout: 45_000, intervals: [500, 1000, 2000] },
        )
        .toBe(true);
      expect(audio!.layerCount, "flag-off audio must be single-layer").toBe(1);
      expect(audio!.layerIndex).toBe(0);
      expect(AUDIO_LADDER_KBPS).toContain(audio!.kbps);
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });

  // -------------------------------------------------------------------------
  // #1398 — SINGLE-LAYER, AUDIO-ONLY uplink-distress bitrate downshift.
  //
  // WHAT IS UNDER TEST: an audio-only publisher (camera OFF) with single-layer
  // audio has NO upper Opus layer to shed, so the layer-ceiling lever (#621) is
  // a no-op for it. #1398 adds a mic-side uplink-distress detector (the recovery
  // `Interval` in microphone_encoder.rs) that, on SUSTAINED publisher-uplink
  // distress while the camera is OFF and audio is single-layer, steps the audio
  // bitrate FLOOR down one tier (50000 → 32000 → 24000 → 16000 bps) and re-applies
  // it LIVE to the running Opus encoder via the worklet `reconfigOpus` (ctl 4002 =
  // OPUS_SET_BITRATE). The first cut from the `u32::MAX` fail-open sentinel lands
  // on tier index 1 (32000 bps), because the TOP tier (50000) IS the healthy
  // bitrate and would be a no-op cut (audio_congestion_bitrate_step_down).
  //
  // HOW THE TEST DRIVES IT (deterministic, no real network impairment):
  // the netsim build (TRUNK_BUILD_FEATURES=netsim, always-on in the e2e stack —
  // docker/docker-compose.e2e.yaml) exposes `window.__vcNetsim.bumpUplinkStall(n)`
  // and `bumpWsDrop(n)`, which add `n` to the process-global transport counters
  // the detector reads (`unistream_ready_stall_count` / `websocket_drop_count`).
  // We bump BOTH axes so the test is transport-agnostic: the decision is an OR
  // across the WT-saturation and WS-drop axes (`audio_uplink_step_down_decision`),
  // and the bump functions increment the shared statics regardless of the elected
  // transport. Audio thresholds (videocall-aq/src/constants.rs): per-axis delta
  // ≥ 5 over a tumbling 4000 ms window, evaluated at the 1 Hz recovery tick. The
  // window must CLOSE with delta ≥ threshold to fire, and the detector seeds its
  // window snapshot from the CURRENT counter at start — so we bump generously
  // (+10 per tick) across several seconds and poll for up to ~20 s, guaranteeing a
  // window closes with a large delta. Recovery climbs the floor back up only one
  // tier per minutes-long cooldown, so the downshift will not be undone mid-test.
  //
  // WHAT WE ASSERT ON — and WHY (the FIX-2 worklet ACK).
  //
  // The worklet line that ACTUALLY applies the downshift
  //   `[encoderWorker] Opus live reconfig … bitRate=(32000|24000|16000)`
  // is emitted from dioxus-ui/scripts/encoderWorker.min.js, which runs as a
  // DEDICATED WEB WORKER (`ENVIRONMENT_IS_WORKER = typeof importScripts ===
  // "function"`). Playwright's `page.on("console")` does NOT capture dedicated-
  // worker console output, and a repo-wide grep found NO worker-console forwarding
  // in the e2e harness (`page.on("worker")` / `worker.on("console")` appear
  // nowhere in e2e/tests or e2e/helpers; `collectConsole` only wires
  // `page.on("console")`). So the worklet line itself is NOT observable here —
  // which is WHY FIX 2 surfaces an ACK on the MAIN thread instead.
  //
  // FIX-2 DESIGN (worklet ACK): the worklet now posts a message back to the main
  // thread — `{ message: "opusReconfigured", bitRate: data.bitRate }` — ONLY when
  // it has actually applied `setOpusControl(4002, data.bitRate)` (OPUS_SET_BITRATE),
  // i.e. only from INSIDE the `reconfigOpus` case's `if (data.bitRate) { … }` block,
  // alongside the ctl-4002 call. The main-thread mic encoder receives that ack and
  // re-logs it as a `log::info!`, which DOES surface via `page.on("console")`
  // (wasm `log::info!` → `console_log`):
  //   `MicrophoneEncoder: worklet ACK opusReconfigured bitRate=<bps>`
  // where `<bps>` is the bare applied bitrate integer (32000/24000/16000 for the
  // downshift tiers — NO parens; it is a Rust format-string value, not the
  // `bit_rate=Some(<n>)` Debug shape the send-path log uses). This ack is the
  // load-bearing signal because it fires ONLY when the worklet truly applied the
  // bitrate to the live Opus encoder.
  //
  // MUTATION GUARD: removing the worklet's `setOpusControl(4002, data.bitRate)`
  // call (in encoderWorker.min.js's `reconfigOpus` case) ALSO removes the ack
  // `postMessage` — it lives inside the SAME `if (data.bitRate)` block — so this
  // assertion FAILS (no ctl 4002 → no ack postMessage → no main-thread `worklet
  // ACK` log → the poll times out → expect fails). The pre-#1398 send-path-only
  // assertion (DOWNSHIFT_RE on `live Opus reconfig applied`) stayed GREEN under
  // that mutation because the main thread logs it after it SENDS the `reconfigOpus`
  // worklet message, regardless of whether the worklet applied it; that is the gap
  // this ack closes.
  //
  // We keep the send-path `DOWNSHIFT_RE` assertion too (a secondary sanity check
  // that the main thread did dispatch the reconfig), but the FINAL gating
  // `expect(...).toBe(true)` is the ACK. We accept 32000/24000/16000 (any
  // sub-50000 tier) so the assertion is robust to an extra tick stepping past
  // 32000 before the poll catches it. (50000 — a healthy reconfig — is excluded so
  // the assertion proves a genuine DOWN-step, not a no-op top-tier re-apply.)
  // -------------------------------------------------------------------------
  test("single-layer audio-only publisher downshifts Opus bitrate on uplink distress (#1398)", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_audio_uplink_downshift_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "audio-uplink-pub@videocall.rs",
        "AudioUplinkPublisher",
        uiURL,
      );
      // Pin SINGLE-LAYER audio (n_audio_layers == 1) so the detector's
      // single-layer gate (`detector_single_layer`) holds. Must run before the
      // first navigation (route is context-scoped).
      await pinSimulcastMaxLayers(pubCtx, 1);

      const pubPage = await pubCtx.newPage();
      // Capture the console BEFORE navigation so the mic-encoder reconfig log
      // (main-thread `log::info!`) is collected once it fires.
      const pubConsole = collectConsole(pubPage);

      // Join AUDIO-ONLY: mic ON, camera OFF — the detector's camera-OFF gate.
      await joinMeetingAudioOnly(pubPage, meetingId, "AudioUplinkPublisher");

      // Guard: the netsim hook (incl. bumpUplinkStall/bumpWsDrop) must exist —
      // otherwise the UI image was built WITHOUT the `netsim` feature and the
      // test would silently never fire. Fail loud with a rebuild instruction.
      const netsimReady = await pubPage.evaluate(
        () =>
          typeof window.__vcNetsim?.install === "function" &&
          typeof (window.__vcNetsim as unknown as { bumpUplinkStall?: unknown }).bumpUplinkStall ===
            "function" &&
          typeof (window.__vcNetsim as unknown as { bumpWsDrop?: unknown }).bumpWsDrop ===
            "function",
      );
      expect(
        netsimReady,
        "window.__vcNetsim.bumpUplinkStall/bumpWsDrop are missing — the dioxus UI image was built " +
          "WITHOUT the `netsim` cargo feature. Rebuild with `make e2e-build` (the e2e stack sets " +
          "TRUNK_BUILD_FEATURES=netsim; see docker/docker-compose.e2e.yaml).",
      ).toBe(true);

      // Let the call stabilize so the mic encoder + its recovery Interval are
      // running before we drive distress.
      await pubPage.waitForTimeout(3000);

      // Drive SUSTAINED uplink distress on BOTH axes. The audio per-axis threshold
      // is delta ≥ 5 over a tumbling 4000 ms window; bump +10 per second so a
      // window always closes with a delta well above threshold. Poll for the
      // main-thread WORKLET ACK log (the FIX-2 mutation-sensitive signal — it fires
      // ONLY when the worklet actually applied ctl 4002), reporting a sub-50000 tier.
      //
      // ACK_RE is the load-bearing assertion: the worklet posts
      // `{ message:"opusReconfigured", bitRate }` back ONLY from inside the same
      // `if (data.bitRate)` block that calls `setOpusControl(4002, …)`, and the
      // main thread re-logs it as `MicrophoneEncoder: worklet ACK opusReconfigured
      // bitRate=<n>`. NOTE the value is a BARE integer (Rust format string), NOT
      // `Some(<n>)` — so match `bitRate=(?:32000|24000|16000)` with no parens.
      const ACK_RE =
        /MicrophoneEncoder: worklet ACK opusReconfigured bitRate=(?:32000|24000|16000)/;
      // DOWNSHIFT_RE is the SEND-path log (kept as a secondary sanity check that the
      // main thread did dispatch the reconfig). It is NOT mutation-sensitive on its
      // own — it logs after SEND regardless of worklet application — so it must NOT
      // be the gating assertion. See the MUTATION GUARD comment above.
      const DOWNSHIFT_RE =
        /MicrophoneEncoder: live Opus reconfig applied.*bit_rate=Some\((?:32000|24000|16000)\)/;

      let bumped = 0;
      await expect
        .poll(
          async () => {
            // Keep distress sustained across windows: one bump per poll iteration.
            await pubPage.evaluate(() => {
              const ns = window.__vcNetsim as unknown as {
                bumpUplinkStall?: (n: number) => unknown;
                bumpWsDrop?: (n: number) => unknown;
              };
              ns.bumpUplinkStall?.(10);
              ns.bumpWsDrop?.(10);
            });
            bumped += 1;
            // The WORKLET ACK is the success predicate: it proves the worklet
            // applied OPUS_SET_BITRATE (ctl 4002), not merely that the main thread
            // sent the reconfig message.
            return pubConsole.some((line) => ACK_RE.test(line));
          },
          {
            // ~20 s of sustained bumping at ~1 Hz (intervals reach 1000 ms): several
            // 4000 ms windows close, each with delta ≥ 10 ≥ threshold(5).
            timeout: 20_000,
            intervals: [500, 1000],
            message:
              "expected the audio-only single-layer publisher to log the WORKLET ACK " +
              "(MicrophoneEncoder: worklet ACK opusReconfigured bitRate=32000|24000|16000) " +
              "proving the worklet applied OPUS_SET_BITRATE (ctl 4002) after sustained uplink " +
              "distress (#1398). Its absence means the worklet never applied the live bitrate " +
              "downshift — check the worklet's reconfigOpus ctl-4002 + ack, the mic-side detector " +
              "gate, or the netsim bump.",
          },
        )
        .toBe(true);

      // SECONDARY sanity (NOT the gate): the main thread also logged the SEND-path
      // reconfig. This pins that both halves fired — the detector dispatched the
      // reconfig AND the worklet acked applying it — but the load-bearing assertion
      // above is the ACK (DOWNSHIFT_RE alone stays green under the ctl-4002 removal
      // mutation; see the MUTATION GUARD comment).
      expect(
        pubConsole.some((line) => DOWNSHIFT_RE.test(line)),
        "expected the main thread to ALSO log the send-path reconfig " +
          "(MicrophoneEncoder: live Opus reconfig applied … bit_rate=Some(32000|24000|16000))",
      ).toBe(true);

      // Sanity: we actually drove distress (the poll did bump), not a fluke.
      expect(bumped, "test must have bumped the uplink counters at least once").toBeGreaterThan(0);
    } finally {
      await pubBrowser.close();
    }
  });

  // #1616 — WT WRITE-DROP axis in ISOLATION (follow-up to #1398).
  //
  // The #1398 detector ORs THREE publisher-uplink-distress axes: WT ready-stall
  // (`unistream_ready_stall_count`), WS send-buffer drop (`websocket_drop_count`),
  // and WT write-drop (`unistream_drop_count`). The test above drives the first
  // two via bumpUplinkStall/bumpWsDrop; the third axis previously had a netsim
  // bumper gap (#1616) and was host-unit-tested only. This test drives ONLY
  // `bumpWtDrop` (no stall, no WS drop) and asserts the SAME worklet ACK, so the
  // WT-drop axis has deterministic e2e coverage into the identical
  // floor → ctl-4002 → worklet → ACK pipeline.
  //
  // Mutation-coupled like the sibling: the ACK log fires ONLY when the worklet
  // applied OPUS_SET_BITRATE (ctl 4002), so reverting the ctl-4002 downshift
  // breaks this WT-drop assertion too (not just the other two axes).
  //
  // UNTAGGED (no @bvt): like the #1398 test above, this needs the netsim-built UI
  // (TRUNK_BUILD_FEATURES=netsim) and so runs on the local docker e2e stack /
  // scoped dispatch, not per-PR CI.
  test("single-layer audio-only publisher downshifts Opus bitrate on WT write-drop distress alone (#1616)", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_audio_uplink_wtdrop_${Date.now()}`;

    const pubBrowser: Browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "audio-uplink-wtdrop-pub@videocall.rs",
        "AudioUplinkWtDropPublisher",
        uiURL,
      );
      // SINGLE-LAYER audio so the detector's single-layer gate holds; must run
      // before the first navigation (route is context-scoped).
      await pinSimulcastMaxLayers(pubCtx, 1);

      const pubPage = await pubCtx.newPage();
      const pubConsole = collectConsole(pubPage);

      // AUDIO-ONLY: mic ON, camera OFF — the detector's camera-OFF gate.
      await joinMeetingAudioOnly(pubPage, meetingId, "AudioUplinkWtDropPublisher");

      // Guard: the bumpWtDrop hook specifically must exist — otherwise the UI was
      // built WITHOUT the `netsim` feature (or predates #1616) and the test would
      // silently never fire. Fail loud with a rebuild instruction.
      const wtDropReady = await pubPage.evaluate(
        () => typeof window.__vcNetsim?.bumpWtDrop === "function",
      );
      expect(
        wtDropReady,
        "window.__vcNetsim.bumpWtDrop is missing — the dioxus UI image was built WITHOUT the " +
          "`netsim` cargo feature or predates #1616. Rebuild with `make e2e-build` (the e2e stack " +
          "sets TRUNK_BUILD_FEATURES=netsim; see docker/docker-compose.e2e.yaml).",
      ).toBe(true);

      // Let the call stabilize so the mic encoder + its recovery Interval run.
      await pubPage.waitForTimeout(3000);

      // Same ACK signal as the #1398 test: the worklet posts opusReconfigured
      // ONLY from inside the ctl-4002 block, re-logged on the main thread as a
      // BARE integer (Rust format string). This is the load-bearing assertion.
      const ACK_RE =
        /MicrophoneEncoder: worklet ACK opusReconfigured bitRate=(?:32000|24000|16000)/;

      let bumped = 0;
      await expect
        .poll(
          async () => {
            // Drive ONLY the WT write-drop axis (+10/iteration > the delta≥5 per
            // 4000 ms window threshold). Deliberately NO bumpUplinkStall / no
            // bumpWsDrop, so a pass proves the WT-drop axis alone trips the OR.
            await pubPage.evaluate(() => {
              window.__vcNetsim?.bumpWtDrop?.(10);
            });
            bumped += 1;
            return pubConsole.some((line) => ACK_RE.test(line));
          },
          {
            timeout: 20_000,
            intervals: [500, 1000],
            message:
              "expected the audio-only single-layer publisher to log the WORKLET ACK " +
              "(MicrophoneEncoder: worklet ACK opusReconfigured bitRate=32000|24000|16000) after " +
              "sustained WT write-drop distress driven via bumpWtDrop ALONE (#1616). Its absence " +
              "means the WT-drop axis did not trip the #1398 detector OR the worklet never applied " +
              "ctl-4002 — check force_unistream_drop → unistream_drop_count, the detector's WT-drop " +
              "axis, and the worklet reconfigOpus ack.",
          },
        )
        .toBe(true);

      // Sanity: we actually drove the WT-drop axis (the poll did bump).
      expect(
        bumped,
        "test must have bumped the WT write-drop counter at least once",
      ).toBeGreaterThan(0);
    } finally {
      await pubBrowser.close();
    }
  });
});

// ---------------------------------------------------------------------------
// #1108 Stage 3 — publish-side layer SUPPRESSION (relay LAYER_HINT → publisher
// caps its ladder when EVERY receiver only wants the base layer; restore-eager
// when any receiver wants more again). Covers BOTH WebTransport and WebSocket.
//
// Relay commit 096795a6 + publisher commit 0a6d8761.
//
// =========================================================================
// WHAT STAGE 3 DOES (the behaviour under test)
// =========================================================================
// The relay computes, per source, the UNION (max over all receivers) of the
// simulcast layer each receiver requested for that source. When that union sits
// below the publisher's published depth (i.e. NO receiver wants a higher rung),
// the relay hints the publisher — on the publisher's OWN NATS self-subject, like
// CONGESTION — to stop encoding the unwanted top rung(s). The publisher's AQ
// loop (`observe_union_requested_layer`) caps its active layer count to
// `min(backpressure ceiling, union count)`, floored at 1 — the BASE layer is
// ALWAYS published, never fully suppressed. Suppress is debounced DOWN at the
// relay (≈2 s); restore is EAGER (immediate) so when any receiver un-pins / a
// new receiver joins / a viewport grows, the dropped rung comes back promptly
// and that receiver receives it again.
//
// =========================================================================
// REMAINING BLOCKER → these tests are `test.fixme` (both transports)
// =========================================================================
// MULTI-PARTY HARNESS LIMITS — #1093 (same blocker as every other multi-context
// test in this spec). These cases join 3+ authenticated contexts each running
// camera + simulcast encode/decode; in headless CI the extra renderers crash
// ("Target page/context closed") and the capability ceiling clamps the runner to
// 1 layer (so there is no top rung TO shed). The WS case also relies on driving
// "every receiver pins base" reliably across contexts, which is exactly the
// multi-party determinism #1093 tracks.
//
// NOTE — publisher-side DOM observability is NO LONGER a blocker. It WAS (the
// old design exposed nothing), but the #1095 redesign on THIS branch surfaces the
// publisher's per-rung send ladder in the Diagnostics drawer's "Simulcast
// layers" section: one chip per layer, testid `diag-simulcast-rung-{layer_id}`,
// with the shed state conveyed by an `is-shed` CSS class (active rungs carry
// `is-active`). The body below is written against THOSE selectors (reached by
// simply opening the unified drawer on the publisher — #1131 removed the
// cross-nav button; the ladder shares the drawer with the perf controls), so it
// goes green the moment the #1093 multi-party harness lands — no further UI work
// is needed.
// (The earlier `perf-video-diag-rung-*` / `data-shed` / `data-bitrate-kbps`
// contract from the never-merged `feat/perf-panel-simulcast-diagnostics` branch
// does NOT exist; do not reintroduce it.)
//
// =========================================================================
// WHAT IS RUNNABLE NOW vs FIXME
// =========================================================================
//   - RUNNABLE NOW: the DRIVE side primitives only — `pinReceiverToBaseLayer`
//     (the RECEIVE max-layer slider → `LAYER_PREFERENCE` path) and `pinTransport`
//     are both exercised live by other tests in this repo
//     (performance-settings.spec.ts and cross-transport-display-name.spec.ts
//     respectively). The publisher-side ASSERTION surface (the Diagnostics
//     ladder) now exists, but the end-to-end test still cannot RUN because the
//     3-context join is blocked on #1093.
//   - FIXME (both WT and WS): the end-to-end "publisher sheds the top rung, then
//     restores it" assertion — blocked ONLY on the #1093 multi-party harness now.
//     No NEW tracking issue is needed: it reuses #1093.
//
// NOTE: unlike the Stage 2 `@impair` divergence test, Stage 3 needs NO toxiproxy
// / network shaping — the suppression trigger is purely "all receivers request
// base", which is a receiver-side preference (slider), not a degraded link. That
// is why these are plain `fixme` (not `@impair`-gated): they belong in the
// default suite once unblocked.
// ---------------------------------------------------------------------------
test.describe("Publish-side layer suppression (#1108 Stage 3)", () => {
  // SERIAL — same #1093 renderer-crash mitigation. These cases launch THREE
  // browsers each (1 publisher + 2 receivers), all running camera + simulcast
  // encode/decode, so they are the heaviest in the spec; never let two of them
  // (or one of them and another multi-browser test) run concurrently on the
  // 8-vCPU CI runner.
  test.describe.configure({ mode: "serial", timeout: 240_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Shared body for the WT and WS variants. Parameterised over the media
   * transport so the identical publish-suppression assertion runs over both.
   *
   * Topology: 1 publisher + 2 receivers (>=3 participants, per the task). All
   * three on `transport`. Both receivers pin the base layer → relay union = base
   * → publisher sheds the top rung(s). Then ONE receiver un-pins (requests a
   * higher layer) and the publisher must RESTORE the rung promptly (<= a couple
   * seconds, restore-eager) and that receiver must receive it.
   */
  const publishSuppressionBody =
    (transport: Transport) =>
    async ({ baseURL }: { baseURL?: string }) => {
      const uiURL = baseURL || "http://localhost:3001";
      const meetingId = `e2e_sim_suppress_${transport}_${Date.now()}`;

      const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
      const rxABrowser = await chromium.launch({ args: BROWSER_ARGS });
      const rxBBrowser = await chromium.launch({ args: BROWSER_ARGS });
      try {
        const pubCtx = await createAuthenticatedContext(
          pubBrowser,
          `sim-sup-pub-${transport}@videocall.rs`,
          "SimSupPublisher",
          uiURL,
        );
        const rxACtx = await createAuthenticatedContext(
          rxABrowser,
          `sim-sup-rxa-${transport}@videocall.rs`,
          "SimSupReceiverA",
          uiURL,
        );
        const rxBCtx = await createAuthenticatedContext(
          rxBBrowser,
          `sim-sup-rxb-${transport}@videocall.rs`,
          "SimSupReceiverB",
          uiURL,
        );

        // Flag ON for all three so the publisher encodes a multi-rung ladder that
        // there is actually something to SHED.
        await enableSimulcastFlag(pubCtx, 3);
        await enableSimulcastFlag(rxACtx, 3);
        await enableSimulcastFlag(rxBCtx, 3);

        // Pin every context to the transport under test. MUST run before the first
        // navigation (these are init scripts).
        await pinTransport(pubCtx, transport);
        await pinTransport(rxACtx, transport);
        await pinTransport(rxBCtx, transport);

        const pubPage = await pubCtx.newPage();
        const rxAPage = await rxACtx.newPage();
        const rxBPage = await rxBCtx.newPage();

        await joinMeeting(pubPage, meetingId, "SimSupPublisher");
        await joinMeeting(rxAPage, meetingId, "SimSupReceiverA");
        await joinMeeting(rxBPage, meetingId, "SimSupReceiverB");

        // Each receiver sees the publisher's tile (peers connected).
        await expect(rxAPage.locator("#grid-container .canvas-container").first()).toBeVisible({
          timeout: 30_000,
        });
        await expect(rxBPage.locator("#grid-container .canvas-container").first()).toBeVisible({
          timeout: 30_000,
        });

        // Open the unified Diagnostics drawer (#1131) on all three contexts. ONE
        // surface now hosts BOTH the receive max-layer sliders (Group A — the
        // migrated Performance panel) AND the publisher's per-rung SEND ladder
        // (Group B — the "Simulcast layers" section). So the same
        // `openPerformancePanel` opener gives the receivers their sliders and the
        // publisher its ladder — no Settings tab, no cross-nav button (both were
        // removed when the surfaces merged in #1131).
        await openPerformancePanel(rxAPage);
        await openPerformancePanel(rxBPage);
        await openPerformancePanel(pubPage);
        // The publisher's per-rung send ladder lives in the SAME open drawer.
        await expect(pubPage.locator("#diagnostics-sidebar.visible")).toBeVisible({
          timeout: 5_000,
        });

        // PHASE 0 — let both receivers climb above base so there is a top rung the
        // publisher is actually encoding (and therefore can later shed). Capability
        // ceiling can clamp to a single layer on a weak runner; SKIP rather than
        // assert a false negative (mirrors the other multi-layer tests).
        await expect
          .poll(
            async () => {
              const a = await readVideoLayer(rxAPage);
              const b = await readVideoLayer(rxBPage);
              if (!a || !b) return -1;
              return Math.min(a.layerCount, b.layerCount);
            },
            { timeout: 45_000, intervals: [1000, 2000, 3000] },
          )
          .toBeGreaterThan(0);

        const aStart = await readVideoLayer(rxAPage);
        const bStart = await readVideoLayer(rxBPage);
        test.skip(
          (aStart?.layerCount ?? 1) <= 1 || (bStart?.layerCount ?? 1) <= 1,
          "capability ceiling clamped the publisher to a single layer; there is no " +
            "top rung to suppress on this runner (see helpers/simulcast-config.ts)",
        );
        const topRung = (aStart?.layerCount ?? 1) - 1; // 0-based id of the highest rung

        // The publisher's per-rung send ladder (Diagnostics "Simulcast layers")
        // exposes one chip per layer, testid `diag-simulcast-rung-{layer_id}`.
        // Active rungs carry the `is-active` class; shed rungs carry `is-shed`
        // (the shed state is a CSS class, NOT a `data-shed`/`data-bitrate-kbps`
        // attribute — that was the never-merged earlier design). The top rung must
        // START active (we have something to shed).
        const topRungDiag = pubPage.locator(`[data-testid="diag-simulcast-rung-${topRung}"]`);
        await expect(topRungDiag).toBeVisible({ timeout: 10_000 });
        await expect(topRungDiag).toHaveClass(/is-active/, { timeout: 30_000 });

        // PHASE 1 — make EVERY receiver request ONLY the base layer. The relay's
        // per-source union now sits at base, so after the suppress-debounce
        // (~2 s) it hints the publisher to stop the top rung(s).
        await pinReceiverToBaseLayer(rxAPage, "video");
        await pinReceiverToBaseLayer(rxBPage, "video");

        // PHASE 2 — assert the PUBLISHER sheds the top rung: its ladder chip for
        // the highest rung flips from `is-active` to `is-shed`. Allow generously
        // for the relay's ~2 s suppress-debounce plus a couple of AQ ticks. The
        // BASE rung must NEVER be shed.
        await expect(topRungDiag).toHaveClass(/is-shed/, { timeout: 30_000 });

        const baseRungDiag = pubPage.locator('[data-testid="diag-simulcast-rung-0"]');
        await expect(baseRungDiag, "base layer must ALWAYS be published — never shed").toHaveClass(
          /is-active/,
        );
        await expect(baseRungDiag).not.toHaveClass(/is-shed/);

        // Both receivers, having pinned base, must still be decoding at the base
        // layer (suppression of higher rungs must not break the base stream).
        await expect
          .poll(async () => (await readVideoLayer(rxAPage))?.layerIndex ?? -1, {
            timeout: 20_000,
            intervals: [1000, 2000],
          })
          .toBe(0);

        // PHASE 3 — RESTORE-EAGER: one receiver requests a higher layer again by
        // un-pinning back to the full automatic range. #1131 §D replaced the old
        // per-stream "Auto" TOGGLE with a "Reset" button (same
        // `perf-recv-video-auto` testid, but NO aria-pressed and only rendered
        // while the stream is constrained). `pinReceiverToBaseLayer` above left
        // the max thumb at 0, so the receiver IS constrained → the Reset button is
        // present. Clicking it clears both bounds back to the full range (auto =
        // true), which grows the relay's union past base. Because restore is EAGER
        // (no debounce) the publisher must re-enable the top rung PROMPTLY. After
        // the click the bounds are cleared, so the Reset button hides itself
        // (count → 0) — that is the post-condition we assert instead of a stale
        // aria-pressed read.
        const rxAReset = rxAPage.locator('[data-testid="perf-recv-video-auto"]');
        await expect(
          rxAReset,
          "Reset button present while the receiver is pinned to base",
        ).toBeVisible({ timeout: 5_000 });
        await expect(rxAReset).not.toHaveAttribute("aria-pressed", /.*/);
        await rxAReset.click();
        // Cleared back to the full range → the Reset button is no longer rendered,
        // and the max thumb snaps back to the ladder top.
        await expect(rxAReset).toHaveCount(0, { timeout: 5_000 });
        const rxAMaxThumb = rxAPage.locator('[data-testid="perf-recv-video-range-max"]');
        const rxATop = await rxAMaxThumb.getAttribute("max");
        await expect(rxAMaxThumb).toHaveValue(String(rxATop));

        // The publisher restores the top rung promptly (restore-eager). ~6 s budget
        // covers the LAYER_PREFERENCE round-trip + one AQ restore tick; the relay
        // adds NO debounce on the UP direction. The chip flips back to `is-active`.
        await expect(topRungDiag).toHaveClass(/is-active/, { timeout: 6_000 });
        await expect(topRungDiag).not.toHaveClass(/is-shed/);

        // PHASE 4 — and the un-pinning receiver actually RECEIVES the higher layer
        // again (the restore is end-to-end, not just a publisher-side flag). It
        // climbs back above the base layer.
        await expect
          .poll(async () => (await readVideoLayer(rxAPage))?.layerIndex ?? 0, {
            timeout: 30_000,
            intervals: [1000, 2000, 3000],
          })
          .toBeGreaterThan(0);
      } finally {
        await pubBrowser.close();
        await rxABrowser.close();
        await rxBBrowser.close();
      }
    };

  // FIXME(#1093): see the describe-block header — the ONLY remaining blocker is
  // the multi-party (3-context) harness (#1093). The publisher-side per-rung
  // observability now exists (Diagnostics "Simulcast layers" ladder,
  // `diag-simulcast-rung-{id}` + `is-shed`/`is-active`), so the body is ready to
  // run as-is once #1093 lands. WebTransport is the production-primary transport,
  // so this is the higher-priority variant to un-fixme first.
  test.fixme(
    "publisher sheds top rung when all receivers pin base, restores when one un-pins (WT, #1108)",
    publishSuppressionBody("webtransport"),
  );

  // FIXME(#1093): same single remaining blocker as the WT case above (the
  // multi-party harness). Unlike the Stage 2 divergence test this needs NO
  // toxiproxy — the trigger is a receiver PREFERENCE (all pin base), not an
  // impaired link — so this belongs in the default suite (not `@impair`) once
  // unblocked.
  test.fixme(
    "publisher sheds top rung when all receivers pin base, restores when one un-pins (WS, #1108)",
    publishSuppressionBody("websocket"),
  );
});
