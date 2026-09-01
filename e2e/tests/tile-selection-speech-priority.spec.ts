import { test, expect, chromium, Page, ConsoleMessage } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Camera-OFF speaker survives the tile cut (issue #2273, PR #2443).
 * Every assertion labels itself in its failure message: DISCRIMINATOR (inverts
 * when the fix is reverted, naming the production file:line it turns on) or
 * SANITY FLOOR (proves the run reached the state where the cut binds, so no
 * discriminator passes vacuously).
 * DELIBERATELY UNTAGGED (no @bvt0 / @bvt1): three Chromium browsers and realtime
 * speech propagation blow the bvt1 smoke budget (playwright.config.ts:57-58), so
 * per-PR CI never runs it — validate with a local docker run or scoped dispatch.
 */

const SPEAKER_NAME = "CutSpeaker";
const SILENT_NAME = "CutSilent";

// Name-scoped on purpose, never positional: two guest tiles are on screen, so a
// `.first()` locator would report state belonging to nobody in particular.
const tileByName = (page: Page, name: string) =>
  page.locator("#grid-container .grid-item", {
    has: page.locator(`h4.floating-name:has-text("${name}")`),
  });

const gridTiles = (page: Page) => page.locator("#grid-container .grid-item");

// `>` is deliberate: the badge (attendants.rs:10549-10554) is a direct sibling
// of the tile loop inside `#grid-container` (attendants.rs:10237), so pinning
// the direct-child edge fails loudly if a wrapper element ever appears.
const overflowBadge = (page: Page) => page.locator("#grid-container > .grid-overflow-badge");

async function waitForNamedTileToSpeak(page: Page, name: string): Promise<void> {
  const tile = tileByName(page, name);
  await expect(
    tile,
    `SANITY FLOOR 2 (not a discriminator): ${name}'s own tile must be on screen before its glow is read`,
  ).toBeVisible({ timeout: 45_000 });

  await expect
    .poll(async () => await readGlow(tile), {
      timeout: 60_000,
      message: `SANITY FLOOR 2 (not a discriminator): ${name}'s tile must enter the speaking-highlight state — the "speaking-tile" class or a non-none inline box-shadow (canvas_generator.rs:953, speak_style()). KNOWN LIMIT, stated rather than hidden: this proves speech reached the host, NOT that the value the ranking reads was written. The glow is written by either peer_tile.rs arm (1588-1597 sender-side VAD, 1615-1637 decoded PCM) while peer_speech_priority has one insert site, the peer_speaking subscriber at attendants.rs:6412-6422. The gap is one-directional — if that insert never happens the DISCRIMINATORS fail loudly, they cannot pass green on un-fixed code`,
    })
    .toBe(true);
}

async function readGlow(tile: ReturnType<typeof tileByName>): Promise<boolean | null> {
  if ((await tile.count()) === 0) {
    return null;
  }
  const className = (await tile.first().getAttribute("class")) || "";
  const style = (await tile.first().getAttribute("style")) || "";
  const hasExplicitGlow = style.includes("box-shadow") && !style.includes("box-shadow: none");
  return className.includes("speaking-tile") || hasExplicitGlow;
}

async function enableMicrophone(page: Page): Promise<void> {
  const mic = page.locator('[data-testid="mic-toggle-button"]');
  await expect(
    mic,
    "the in-meeting mic control must render (stable hook declared at video_control_buttons.rs:69)",
  ).toBeVisible({ timeout: 20_000 });

  const label = (await mic.getAttribute("aria-label")) || "";
  if (label.includes("Unmute")) {
    await mic.click();
  }
  await expect(
    mic,
    'the mic must report the LIVE aria-label (video_control_buttons.rs:51-57). "Microphone — Unmute" means still muted and "Microphone unavailable — click to retry." means the device was never acquired; either would silently turn this into a two-silent-peer run in which the ranking has nothing to rank',
  ).toHaveAttribute("aria-label", "Microphone — Mute", { timeout: 15_000 });
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult !== "waiting") {
    return;
  }
  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(
    admitButton,
    "the waiting-room Admit control must be present; `.first()` is unambiguous here only because the caller admits guests strictly one at a time, waiting for each tile before the next guest navigates",
  ).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
  await hostPage.waitForTimeout(3000);

  const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const guestGrid = guestPage.locator("#grid-container");
  const postAdmit = await Promise.race([
    guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
    guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
  ]);
  if (postAdmit === "join-button") {
    await guestPage.waitForTimeout(1000);
    await guestJoinButton.click();
    await guestPage.waitForTimeout(3000);
    await expect(guestGrid, "the admitted guest must land in the meeting grid").toBeVisible({
      timeout: 15_000,
    });
  }
}

function collectConsole(page: Page, label: string, sink: string[]): void {
  page.on("console", (msg: ConsoleMessage) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      sink.push(`[${label}:${msg.type()}] ${msg.text()}`);
    }
  });
}

test.describe("Tile selection ranks speech above join order (#2273)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("camera-off speaker keeps the surviving grid cell when the tile cut binds", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `tile_speech_priority_${Date.now()}`;
    const consoleLines: string[] = [];

    // Swept-amplitude tone, not flat: it holds the receiver's VAD verdict steady
    // instead of flapping or lapsing, and loops without a pitch discontinuity.
    const toneWav = continuousToneWavPath();

    const browserHost = await chromium.launch({ args: BROWSER_ARGS });
    const browserSilent = await chromium.launch({ args: BROWSER_ARGS });
    const browserSpeaker = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${toneWav}`],
    });

    const hostCtx = await createAuthenticatedContext(
      browserHost,
      "cuthost@videocall.rs",
      "CutHost",
      uiURL,
    );
    // Seeds are the CURRENT read paths: vc_prejoin_camera_on / vc_prejoin_mic_on
    // are plain "true"/"false", both defaulting to FALSE (context.rs:1132-1146).
    // Density and dock are pinned so the tile-width floor and padding branch the
    // lever depends on are facts, not defaults (context.rs:172-186, 106-120).
    await hostCtx.addInitScript(
      `localStorage.setItem("vc_prejoin_camera_on", "false");` +
        `localStorage.setItem("vc_prejoin_mic_on", "false");` +
        `localStorage.setItem("vc_density_mode", "standard");` +
        `localStorage.setItem("vc_dock_position", "bottom");`,
    );

    const silentCtx = await createAuthenticatedContext(
      browserSilent,
      "cutsilent@videocall.rs",
      SILENT_NAME,
      uiURL,
    );
    await silentCtx.addInitScript(
      `localStorage.setItem("vc_prejoin_camera_on", "false");` +
        `localStorage.setItem("vc_prejoin_mic_on", "false");`,
    );

    const speakerCtx = await createAuthenticatedContext(
      browserSpeaker,
      "cutspeaker@videocall.rs",
      SPEAKER_NAME,
      uiURL,
    );
    await speakerCtx.addInitScript(
      `localStorage.setItem("vc_prejoin_camera_on", "false");` +
        `localStorage.setItem("vc_prejoin_mic_on", "true");`,
    );

    const hostPage = await hostCtx.newPage();
    const silentPage = await silentCtx.newPage();
    const speakerPage = await speakerCtx.newPage();

    collectConsole(hostPage, "host", consoleLines);
    collectConsole(silentPage, SILENT_NAME, consoleLines);
    collectConsole(speakerPage, SPEAKER_NAME, consoleLines);

    try {
      await hostPage.setViewportSize({ width: 1280, height: 800 });

      await fillAndSubmitJoinForm(hostPage, meetingId, "CutHost");
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(
        hostResult,
        "the observer must reach the meeting — every assertion in this spec reads its grid",
      ).toBe("in-meeting");

      await fillAndSubmitJoinForm(silentPage, meetingId, SILENT_NAME);
      const silentResult = await joinMeetingFromPage(silentPage);
      await admitGuestIfNeeded(hostPage, silentPage, silentResult);
      await expect(
        tileByName(hostPage, SILENT_NAME),
        `${SILENT_NAME} must be registered on the host BEFORE ${SPEAKER_NAME} navigates. Join time is stamped from the host's own clock in on_peer_added (attendants.rs:4464), so this wait is what makes "the silent guest joined earlier" a fact instead of a race`,
      ).toBeVisible({ timeout: 60_000 });

      await fillAndSubmitJoinForm(speakerPage, meetingId, SPEAKER_NAME);
      const speakerResult = await joinMeetingFromPage(speakerPage);
      await admitGuestIfNeeded(hostPage, speakerPage, speakerResult);

      await enableMicrophone(speakerPage);

      await expect(
        tileByName(hostPage, SILENT_NAME),
        "SANITY FLOOR 1 (not a discriminator): both camera-off guests must be on screen at the roomy baseline. The viewport is set explicitly above because a manually-created context does NOT inherit the config's Desktop Chrome viewport. Note this spec asserts NO decoded remote canvas anywhere — every tile here is a camera-OFF avatar that needs none, and issue #2193 makes a canvas precondition unpassable in this environment",
      ).toBeVisible({ timeout: 60_000 });
      await expect(
        tileByName(hostPage, SPEAKER_NAME),
        "SANITY FLOOR 1 (not a discriminator): both camera-off guests must be on screen at the roomy baseline",
      ).toBeVisible({ timeout: 60_000 });
      await expect(
        gridTiles(hostPage),
        "SANITY FLOOR 1 (not a discriminator): exactly two tiles are placed and NOTHING has been truncated yet, so the identity assertions at the end cannot pass in a layout that never cut anything",
      ).toHaveCount(2, { timeout: 30_000 });
      await expect(
        overflowBadge(hostPage),
        "SANITY FLOOR 1 (not a discriminator): no overflow badge before the lever is pulled",
      ).toHaveCount(0, { timeout: 15_000 });

      await waitForNamedTileToSpeak(hostPage, SPEAKER_NAME);

      const silentGlow = await readGlow(tileByName(hostPage, SILENT_NAME));
      expect(
        silentGlow,
        `SANITY FLOOR 2b (not a discriminator, guards the SETUP): readGlow returns null — never false — for a tile that is not in the DOM, so reject null FIRST; a detached ${SILENT_NAME} tile must never be read as a quiet one`,
      ).not.toBeNull();
      expect(
        silentGlow,
        `SANITY FLOOR 2b (not a discriminator, guards the SETUP): ${SILENT_NAME} joined with vc_prejoin_mic_on false and must stay dark. A leaked fake microphone would give BOTH peers recent speech, the fixed ranking would fall through to its freshness/join tiebreak (attendants_layout.rs:175-181), and the discriminators below would become ambiguous`,
      ).toBe(false);

      // THE LEVER — viewport only, no mock peers and no production change: it
      // squeezes off_to_render to 1 while camera_off_real still holds 2 entries.
      await hostPage.setViewportSize({ width: 400, height: 340 });

      await expect(
        gridTiles(hostPage),
        "SANITY FLOOR 3 (not a discriminator): the cut must ACTUALLY bind. At 400x340 the under-568px tile-width floor is 250 (density.rs:21-39) and the bottom dock pads 8/8/80/8 (attendants.rs:8173-8180), so compute_layout(2, 384, 252, 8) (attendants_layout.rs:17-49) picks cols=2 at 188px — under the floor — effective_visible walks down to 1 (attendants.rs:8258-8268) and displayed_tile_count becomes 1 (attendants.rs:8357-8362). One cell for two camera-off peers is what stops the identity assertions below from passing vacuously",
      ).toHaveCount(1, { timeout: 30_000 });
      await expect(
        overflowBadge(hostPage),
        "SANITY FLOOR 3 (not a discriminator): exactly one peer must be shed into the overflow badge (attendants.rs:10549-10554)",
      ).toHaveCount(1, { timeout: 30_000 });
      await expect(
        overflowBadge(hostPage),
        "SANITY FLOOR 3 (not a discriminator): the badge must count exactly one overflowed peer",
      ).toContainText("+1", { timeout: 15_000 });

      await expect(
        tileByName(hostPage, SPEAKER_NAME),
        `DISCRIMINATOR A: the SPEAKING guest must hold the one surviving cell. The cut is positional — camera_off_real.take(off_to_render) at attendants.rs:8595-8601 — so the sort immediately before it decides who survives. On base 77f84269 that sort was join-time ascending (attendants.rs:8408-8417): camera_off_real is [${SILENT_NAME}, ${SPEAKER_NAME}] because the speaker joined LAST, take(1) keeps ${SILENT_NAME}, and this assertion FAILS with count 0. With the fix, sort_camera_off_window ranks recent speech first (attendants.rs:8416-8426 -> attendants_layout.rs:161-183; sb.is_some().cmp(&sa.is_some()) at 173-174; the recency window is SPEAKER_ACTIVE_MS = 30s at 96-106), so take(1) keeps ${SPEAKER_NAME}`,
      ).toHaveCount(1, { timeout: 30_000 });
      await expect(
        tileByName(hostPage, SPEAKER_NAME),
        'DISCRIMINATOR A: nothing else in the PR can rescue this tile, so a surviving VISIBLE speaker is attributable to sort_camera_off_window alone — promote_speakers runs over camera-ON reals plus mocks, which is empty here (rendered_on = 0, attendants.rs:8595), and select_display_candidates early-returns its input untouched when capped_real >= display_peers.len() (attendants_layout.rs:121-126), both being 2 here. The lone survivor renders full_bleed, whose class is built as "grid-item full-bleed" (canvas_generator.rs:1633-1636), so the .grid-item locator still matches it',
      ).toBeVisible({ timeout: 15_000 });

      await expect(
        tileByName(hostPage, SILENT_NAME),
        `DISCRIMINATOR B: the SILENT earliest joiner must be the one shed. Mirror of A and not redundant with it — A alone would still pass if the grid rendered BOTH tiles and B alone would still pass if it rendered NEITHER, so with SANITY FLOOR 3 the pair pins the single surviving cell to one named peer. Un-fixed, ${SILENT_NAME} is the survivor and this FAILS with count 1`,
      ).toHaveCount(0, { timeout: 30_000 });
    } finally {
      if (consoleLines.length > 0) {
        console.log(`[#2273] browser console (last 40 error/warning lines):`);
        console.log(consoleLines.slice(-40).join("\n"));
      }
      await browserHost.close();
      await browserSilent.close();
      await browserSpeaker.close();
    }
  });
});
