import { readFileSync } from "node:fs";
import path from "node:path";

import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { wakeControls } from "../helpers/controls";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { HEARTBEAT_KEEPALIVE_INTERVAL_MS } from "../helpers/rust-mirrored-constants";
import {
  GlowSample,
  classifyGlow,
  describeSample,
  startGlowTimeline,
  stopGlowTimeline,
} from "../helpers/speaking-glow";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import {
  ALWAYS_SPEAKING_VAD_THRESHOLD,
  NEVER_SPEAKING_VAD_THRESHOLD,
  setVadThreshold,
} from "../helpers/vad-threshold-config";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2660 — a peer we CAN hear, whose heartbeat claims speech but whose audio
 * this client's decoder never reports as speech, must not be lit by that claim.
 *
 * FIXED: liveness true + verdict `Unheard` -> each keepalive folds to
 * `Some(false)` -> rule 1 -> `Some(0.0)`, dark throughout. UN-FIXED (between
 * `a22100a2` and `76bd4488`): no fold, so the first heartbeat takes rule 3's
 * dark branch, writes `HEARTBEAT_SOURCED_GLOW_LEVEL`, and later ones re-arm the
 * deadman — lit throughout, and `waitForGlow(.., "silent", ..)` times out there.
 *
 * The tone must stay LOUD and UNBROKEN: silence engages DTX, the buffer drains
 * to its 7 ms residue, liveness lapses, and the tile then CORRECTLY relights
 * under 2174 (a silence fixture is parked on `wip/2660-tile-glow-spec`).
 *
 * UNMEASURED: that a continuously-transmitting sender holds `audio_buffer_ms`
 * above the floor is argued, not measured — the `assertAudioIsArriving` calls
 * ARE that measurement. Never executed; untagged, so per-PR CI skips it too.
 */

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const PEER_TILE_RS = "dioxus-ui/src/components/peer_tile.rs";

/** Read, not mirrored: `peer_tile.rs` is absent from the lint workflow's `paths:`. */
function rustNumericConst(rel: string, name: string): number {
  const abs = path.resolve(REPO_ROOT, rel);
  let src: string;
  try {
    src = readFileSync(abs, "utf8");
  } catch (err) {
    throw new Error(`cannot read ${rel} (resolved ${abs}) while reading ${name}`, { cause: err });
  }
  const re = new RegExp(
    `^(?:pub(?:\\([a-z]+\\))? )?const ${name}\\s*:\\s*[A-Za-z0-9_]+\\s*=\\s*([^;]+);`,
    "gm",
  );
  const hits = [...src.matchAll(re)];
  if (hits.length !== 1) {
    throw new Error(
      `expected exactly 1 top-level \`const ${name}\` in ${rel}, found ${hits.length} — ` +
        `renamed, moved or redefined; re-point this reader before trusting the assertion on it`,
    );
  }
  const raw = hits[0][1].trim().replace(/_/g, "");
  if (!/^-?\d+(\.\d+)?$/.test(raw)) {
    throw new Error(
      `\`const ${name}\` in ${rel} is \`${raw}\`, not a plain number literal; this reader only ` +
        `resolves literals, so ${name} must be derived explicitly here`,
    );
  }
  return Number(raw);
}

const LIVE_BUFFER_FLOOR_MS = rustNumericConst(PEER_TILE_RS, "LIVE_BUFFER_FLOOR_MS");

const GLOW_DEADMAN_MS = (HEARTBEAT_KEEPALIVE_INTERVAL_MS * 5) / 2;

const KEEPALIVES_OBSERVED = 3;

const OBSERVE_MS =
  Math.max(KEEPALIVES_OBSERVED * HEARTBEAT_KEEPALIVE_INTERVAL_MS, GLOW_DEADMAN_MS) + 3_000;

const SETTLE_DARK_TIMEOUT_MS = 2 * HEARTBEAT_KEEPALIVE_INTERVAL_MS + 10_000;

const MIN_SAMPLES = Math.floor(OBSERVE_MS / 50 / 2);

// Load-bearing: glow off makes `speak_style` always silent, so "dark
// throughout" would be vacuous. Anything but "false" enables.
const APPEARANCE_SEED_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_appearance_glow_enabled", "true");
  } catch (_) {}
})();`;

/** By testid, not tooltip text: `hasText` is a substring match, so "Mute" also matches UNMUTE. */
function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

async function enableMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

async function muteMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
  await toggle.click();
  await expect(toggle).toHaveClass(/\boff\b/, { timeout: 15_000 });
}

/**
 * Self-view is the `Host` component, not a `.grid-item`, so the count assert
 * catches a layout change. `[data-mic-muted]` is a DESCENDANT, not a sibling.
 */
async function remoteTile(hostPage: Page): Promise<{ tile: Locator; tileId: string }> {
  const tiles = hostPage.locator("#grid-container .grid-item");
  await expect(tiles.first()).toBeVisible({ timeout: 30_000 });
  await expect(tiles, "expected exactly one remote tile (the guest) in the grid").toHaveCount(1);
  const tile = tiles.first();

  const tileId = await tile.getAttribute("id");
  expect(tileId, "the guest tile needs a stable id to track across renders").toBeTruthy();

  await expect(tile.locator("[data-mic-muted]").first()).toHaveAttribute(
    "data-mic-muted",
    "false",
    { timeout: 30_000 },
  );

  return { tile, tileId: tileId as string };
}

async function waitForGlow(
  tile: Locator,
  verdict: "lit" | "silent",
  timeout: number,
  message: string,
): Promise<void> {
  await expect
    .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), { timeout, message })
    .toBe(verdict);
}

/**
 * Hover target is an unclassed `div` marked only by inline `cursor: crosshair` —
 * the weakest selector here; a `data-testid` in `signal_quality.rs` is the fix.
 */
async function readAudioBufferMs(hostPage: Page, tile: Locator): Promise<number> {
  const signalButton = tile.locator("button.signal-indicator").first();
  await expect(signalButton).toBeVisible({ timeout: 15_000 });
  await signalButton.click();

  const popup = hostPage.locator(".signal-quality-popup").first();
  await expect(popup, "the signal popup did not open").toBeVisible({ timeout: 15_000 });

  const chartOverlay = popup.locator('div[style*="cursor: crosshair"]').first();
  await expect(
    chartOverlay,
    "the signal popup never rendered its chart overlay — either no samples have accumulated " +
      "yet, or the overlay this reader hovers has changed shape",
  ).toBeVisible({ timeout: 30_000 });

  let text = "";
  await expect
    .poll(
      async () => {
        await chartOverlay.hover();
        // `evaluate`, not a locator: the tooltip is lazily created and
        // `textContent()` rejects on a missing element, which a poll propagates.
        text = await hostPage.evaluate(
          () => document.getElementById("signal-chart-tooltip-global")?.textContent ?? "",
        );
        return /Audio: buf \d+ms/.test(text);
      },
      {
        timeout: 20_000,
        message:
          "the signal-chart tooltip never rendered an `Audio: buf {N}ms` line — without it " +
          "there is no evidence audio is arriving, and a dark tile proves nothing",
      },
    )
    .toBe(true);

  const match = /Audio: buf (\d+)ms/.exec(text);
  if (!match) {
    throw new Error(`tooltip matched the probe but not the capture — text was: ${text}`);
  }
  const bufferMs = Number(match[1]);

  await popup.locator("button.popup-close").click();
  await expect(popup, "the signal popup did not close").toBeHidden({ timeout: 10_000 });

  return bufferMs;
}

async function assertAudioIsArriving(hostPage: Page, tile: Locator, when: string): Promise<number> {
  const bufferMs = await readAudioBufferMs(hostPage, tile);
  expect(
    bufferMs,
    `${when}: the peer's decoded-audio buffer read ${bufferMs}ms, under the ` +
      `${LIVE_BUFFER_FLOOR_MS}ms floor \`records_live_audio\` gates on. The liveness gate is ` +
      `therefore NOT held open, the veto under test would not fire, and a dark tile would mean ` +
      `"we cannot hear this peer" — the issue-2174 case — rather than the issue-2660 one`,
  ).toBeGreaterThanOrEqual(LIVE_BUFFER_FLOOR_MS);
  return bufferMs;
}

function assertTileNeverLit(samples: GlowSample[]): void {
  expect(
    samples.length,
    "too few samples — the recorder was not running for the measured window",
  ).toBeGreaterThan(MIN_SAMPLES);
  expect(
    samples.filter((s) => s.missing).length,
    "the tracked tile disappeared mid-window — the recording is not meaningful",
  ).toBe(0);

  const unknown = samples.filter((s) => classifyGlow(s.style) === "unknown");
  expect(
    unknown.length,
    `${unknown.length} sample(s) carried a style this spec cannot classify as speak_style ` +
      `output — first: ${describeSample(unknown[0])}. An unrecognised style is never counted ` +
      `as evidence, so this would silently weaken the assertion below`,
  ).toBe(0);

  const muted = samples.filter((s) => s.muted === "true");
  expect(
    muted.length,
    `this client rendered the peer as MUTED in ${muted.length} sample(s); a muted peer's glow is ` +
      `held dark by the audio-off veto, so the window proves nothing about issue 2660`,
  ).toBe(0);
  expect(
    samples.filter((s) => s.muted === "false").length,
    "no sample carried data-mic-muted at all — the mic glyph was never rendered, so the " +
      "audio-enabled check above passed on an empty set",
  ).toBeGreaterThan(MIN_SAMPLES);

  const lit = samples.filter((s) => classifyGlow(s.style) === "lit");
  expect(
    lit.length,
    `the speaking glow was lit in ${lit.length}/${samples.length} samples across ${OBSERVE_MS}ms ` +
      `(${KEEPALIVES_OBSERVED}+ keepalives of ${HEARTBEAT_KEEPALIVE_INTERVAL_MS}ms, and longer ` +
      `than the ${GLOW_DEADMAN_MS}ms deadman) on a peer whose audio IS arriving and whose ` +
      `decoder has never reported speech. Issue 2660: rule 3 raised the heartbeat's claim to ` +
      `HEARTBEAT_SOURCED_GLOW_LEVEL on the dark tile, and every later keepalive re-armed the ` +
      `deadman that should have retired it. First lit sample: ${describeSample(lit[0])}`,
  ).toBe(0);
}

test.describe("An unheard peer's heartbeat must not light a tile we can hear", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a peer whose audio arrives but never reads as speech stays dark across keepalives", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_unheard_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: [...BROWSER_ARGS] });
    const guestBrowser = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`],
    });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "unheardhost@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "unheardguest@videocall.rs",
        "GuestUser",
        uiURL,
      );

      // Before either context has a page: `RuntimeConfig` is memoized on first parse.
      await setVadThreshold(guestCtx, ALWAYS_SPEAKING_VAD_THRESHOLD);
      await setVadThreshold(hostCtx, NEVER_SPEAKING_VAD_THRESHOLD);
      await hostCtx.addInitScript(APPEARANCE_SEED_INIT_SCRIPT);
      await enableDiagnosticsTileIndicators(hostCtx);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await enterTwoUserMeeting(hostPage, guestPage, meetingId);
      await enableMic(guestPage);

      const { tile, tileId } = await remoteTile(hostPage);

      // Precondition: we can hear this peer, else this is the 2174 case.
      const bufferBefore = await assertAudioIsArriving(hostPage, tile, "before the window");

      await waitForGlow(
        tile,
        "silent",
        SETTLE_DARK_TIMEOUT_MS,
        `the guest tile never went dark within ${SETTLE_DARK_TIMEOUT_MS}ms of audio being live ` +
          `(buffer read ${bufferBefore}ms). On an un-fixed build this is exactly the failure ` +
          `expected: a heartbeat claiming is_speaking=1 lights an unheard peer through rule 3 ` +
          `and nothing retires it`,
      );

      await startGlowTimeline(hostPage, tileId);
      await hostPage.waitForTimeout(OBSERVE_MS);
      const samples = await stopGlowTimeline(hostPage);
      assertTileNeverLit(samples);

      // Both: `.speaking-tile` sets `border-color: transparent`, which the
      // inline style overrides, so neither stands alone.
      await expect(tile).not.toHaveClass(/speaking-tile/, { timeout: 10_000 });
      expect(classifyGlow((await tile.getAttribute("style")) || "")).toBe("silent");

      // Twice, so the precondition is shown to have held THROUGH the window.
      await assertAudioIsArriving(hostPage, tile, "after the window");

      // Only the `peer_status` arm writes `data-mic-muted`, so without this
      // "no heartbeat lit the tile" is trivially true.
      await muteMic(guestPage);
      await expect(
        tile.locator("[data-mic-muted]").first(),
        "the host never learned the guest had muted — no peer_status heartbeat reached this " +
          "client, so the dark tile above cannot be attributed to the fold under test",
      ).toHaveAttribute("data-mic-muted", "true", { timeout: 30_000 });
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
