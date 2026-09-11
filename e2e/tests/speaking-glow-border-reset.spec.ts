import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { toneThenSilenceWavPath } from "../helpers/audio-fixtures";
import { wakeControls } from "../helpers/controls";
import { classifyGlow } from "../helpers/speaking-glow";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2660 — a peer tile's border must return to rest when the peer stops
 * speaking. dioxus-interpreter-js `set_attribute.ts` (`case "style"`) snapshots
 * the border longhands, writes the new attribute, then re-applies every
 * longhand that now reads back empty — and one set through a `var()` SHORTHAND
 * does read back empty. So a `border-color: var(..)` shorthand reset in
 * `speak_style` was overwritten with the four lit colours it had just replaced;
 * four `border-*-color` longhands are not. RED un-fixed at the reset assertion
 * and nowhere earlier: `borderTopColor stayed rgb(91, 207, 159)` (default mint).
 */

const POLL_INTERVAL_MS = 250;
const LIT_TIMEOUT_MS = 30_000;
const SILENT_TIMEOUT_MS = 35_000;
const RESET_TIMEOUT_MS = 15_000;
const HOLD_MS = 5_000;
const HOLD_INTERVAL_MS = 500;

// Forces the glow ON: with the setting off `speak_style` takes a silent branch
// unconditionally, and the positive control below would be vacuous.
const APPEARANCE_SEED_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_appearance_glow_enabled", "true");
  } catch (_) {}
})();`;

interface BorderProbe {
  style: string;
  borderTop: string;
  borderBottom: string;
  restingToken: string;
  resting: string;
  shorthandColour: string | null;
  missing: boolean;
}

/** One atomic read: the lit colour tracks the peer's audio level, so reading
 * the style and the computed colour in two round trips compares two frames. */
async function readBorder(page: Page, tileId: string): Promise<BorderProbe> {
  return page.evaluate((id: string): BorderProbe => {
    const el = document.getElementById(id);
    if (!el) {
      return {
        style: "",
        borderTop: "",
        borderBottom: "",
        restingToken: "",
        resting: "",
        shorthandColour: null,
        missing: true,
      };
    }

    const probe = document.createElement("div");
    document.body.appendChild(probe);
    const normalise = (raw: string): string => {
      probe.style.removeProperty("border-top-color");
      probe.style.setProperty("border-top-color", raw.trim());
      return getComputedStyle(probe).borderTopColor;
    };

    const cs = getComputedStyle(el);
    const style = el.getAttribute("style") || "";
    // `(?:^|;)` plus the colon keep this off `transition: border-color 0.3s`.
    const shorthand = /(?:^|;)\s*border-color\s*:\s*([^;]+)/.exec(style);
    const restingToken = cs.getPropertyValue("--grid-item-border").trim();

    const sample: BorderProbe = {
      style,
      borderTop: cs.borderTopColor,
      borderBottom: cs.borderBottomColor,
      restingToken,
      resting: restingToken === "" ? "" : normalise(restingToken),
      shorthandColour: shorthand ? normalise(shorthand[1]) : null,
      missing: false,
    };
    probe.remove();
    return sample;
  }, tileId);
}

/** Returns the LAST sample either way, so a failure message quotes the real one. */
async function pollBorder(
  page: Page,
  tileId: string,
  ready: (s: BorderProbe) => boolean,
  timeoutMs: number,
): Promise<BorderProbe> {
  const deadline = Date.now() + timeoutMs;
  let sample = await readBorder(page, tileId);
  while (!ready(sample) && Date.now() < deadline) {
    await page.waitForTimeout(POLL_INTERVAL_MS);
    sample = await readBorder(page, tileId);
  }
  return sample;
}

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

async function remoteTile(hostPage: Page): Promise<{ tile: Locator; tileId: string }> {
  const tiles = hostPage.locator("#grid-container .grid-item");
  await expect(tiles.first()).toBeVisible({ timeout: 30_000 });
  await expect(tiles, "expected exactly one remote tile (the guest) in the grid").toHaveCount(1);
  const tile = tiles.first();

  const tileId = await tile.getAttribute("id");
  expect(tileId, "the guest tile needs a stable id to track across renders").toBeTruthy();
  return { tile, tileId: tileId as string };
}

test.describe("A peer tile's border returns to rest when the peer stops speaking", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the speaking border resets to --grid-item-border after the peer falls silent @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_border_reset_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: [...BROWSER_ARGS] });
    const guestBrowser = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${toneThenSilenceWavPath()}`],
    });

    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "borderresethost@videocall.rs",
        "HostUser",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "borderresetguest@videocall.rs",
        "GuestUser",
        uiURL,
      );
      await hostCtx.addInitScript(APPEARANCE_SEED_INIT_SCRIPT);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await enterTwoUserMeeting(hostPage, guestPage, meetingId);
      const { tile, tileId } = await remoteTile(hostPage);

      await enableMic(guestPage);
      await expect(
        tile.locator("[data-mic-muted]").first(),
        "the host never learned the guest had unmuted, so nothing below is about this guest's audio",
      ).toHaveAttribute("data-mic-muted", "false", { timeout: 30_000 });

      const opening = await readBorder(hostPage, tileId);
      expect(
        opening.restingToken,
        "the tile resolves no `--grid-item-border` — the token this spec compares against has " +
          "been renamed or removed, so every comparison below would run against `currentcolor`",
      ).not.toBe("");

      const lit = await pollBorder(
        hostPage,
        tileId,
        (s) =>
          !s.missing &&
          classifyGlow(s.style) === "lit" &&
          s.shorthandColour !== null &&
          s.borderTop === s.shorthandColour,
        LIT_TIMEOUT_MS,
      );
      expect(lit.missing, "the tracked tile vanished before a lit border could be measured").toBe(
        false,
      );
      expect(
        [classifyGlow(lit.style), lit.borderTop],
        `the guest tile never rendered a lit border matching its own inline glow colour within ` +
          `${LIT_TIMEOUT_MS}ms — either the peer's speech never reached this client, or the ` +
          `fixture's tone ended before the lit border was observed. Either way the reset under ` +
          `test would go untested. Inline style: ${lit.style}`,
      ).toEqual(["lit", lit.shorthandColour]);
      const glowColour = lit.borderTop;
      expect(glowColour, "the lit colour must differ from the resting colour").not.toBe(
        lit.resting,
      );

      const silent = await pollBorder(
        hostPage,
        tileId,
        (s) => classifyGlow(s.style) === "silent",
        SILENT_TIMEOUT_MS,
      );
      expect(
        classifyGlow(silent.style),
        `the guest tile never went silent within ${SILENT_TIMEOUT_MS}ms of its border lighting — ` +
          `the fixture's tone ends inside that window, so the peer's silence never reached this ` +
          `client. Inline style: ${silent.style}`,
      ).toBe("silent");

      // THE REGRESSION ASSERTION.
      const reset = await pollBorder(
        hostPage,
        tileId,
        (s) => !s.missing && s.borderTop === s.resting && s.borderBottom === s.resting,
        RESET_TIMEOUT_MS,
      );
      expect(reset.missing, "the tracked tile vanished before the reset could be measured").toBe(
        false,
      );
      expect(
        [reset.borderTop, reset.borderBottom],
        `issue 2660: the tile is silent but its computed border never returned to rest within ` +
          `${RESET_TIMEOUT_MS}ms. borderTopColor stayed ${reset.borderTop}, the glow colour being ` +
          `${glowColour}, where --grid-item-border resolves to ${reset.resting}. Inline style: ` +
          `${reset.style}`,
      ).toEqual([reset.resting, reset.resting]);

      // ...and it holds, so one transient sample cannot satisfy the poll above.
      const holdUntil = Date.now() + HOLD_MS;
      while (Date.now() < holdUntil) {
        await hostPage.waitForTimeout(HOLD_INTERVAL_MS);
        const held = await readBorder(hostPage, tileId);
        expect(held.missing, "the tracked tile disappeared during the hold window").toBe(false);
        expect(
          [held.borderTop, held.borderBottom],
          `the border left its resting colour again during the ${HOLD_MS}ms hold, with the tile ` +
            `${classifyGlow(held.style)}. Inline style: ${held.style}`,
        ).toEqual([held.resting, held.resting]);
      }
    } finally {
      await hostBrowser.close();
      await guestBrowser.close();
    }
  });
});
