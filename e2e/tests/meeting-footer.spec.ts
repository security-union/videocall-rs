import { readFileSync } from "node:fs";
import path from "node:path";
import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { setShowBuildGitInfoFlag } from "../helpers/show-build-git-info-config";
import { openPeerList } from "../helpers/controls";
import { MEETING_FOOTER } from "../helpers/rust-mirrored-constants";

/**
 * The in-call meeting footer and its "Meeting info" dialog (issue 2791).
 *
 * The footer is the last child of the in-call `#main-container`: one
 * `#meeting-footer-trigger` button over an aria-hidden line. The dialog renders
 * outside `#main-container`, so neither the background light-dismiss nor the
 * Escape chain there sees its events. Only the open -> Escape -> focus-return
 * core is `@bvt1`; the rest runs under `--project=dioxus`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const DESKTOP = { width: 1280, height: 720 };
const EPS = 1;
const FOOTER_H = MEETING_FOOTER.MEETING_FOOTER_RESERVE;

const FOOTER = '[data-testid="meeting-footer"]';
const TRIGGER_ID = "meeting-footer-trigger";
const TRIGGER = `#${TRIGGER_ID}`;
const DIALOG = "#meeting-info-dialog";
const BACKDROP_TESTID = "meeting-info-dialog-backdrop";
const BACKDROP = `[data-testid="${BACKDROP_TESTID}"]`;
const MEETING_ID_VALUE = '[data-testid="meeting-info-row-meeting-id"] .meeting-info-value--id';
const COPY_TESTID = "meeting-info-copy-link";
const COPY_BUTTON = `[data-testid="${COPY_TESTID}"]`;
const COPY_FAILED_ANNOUNCEMENT = "Couldn't copy — select the link text";
const DOCK = ".video-controls-container";
const GRID = "#grid-container";
const GRID_TILES = "#grid-container > .grid-item";
const PEER_LIST = "#peer-list-container";
const TIMER_RE = /^\d{2}:\d{2}(:\d{2})?$/;

function readClientVersionFromCargoToml(): string {
  const cargoTomlPath = path.resolve(__dirname, "../../dioxus-ui/Cargo.toml");
  const match = readFileSync(cargoTomlPath, "utf8").match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m);
  if (!match) {
    throw new Error(`Could not parse version from ${cargoTomlPath}`);
  }
  return match[1];
}

const CLIENT_VERSION = readClientVersionFromCargoToml();

interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

async function rectOf(locator: Locator, what: string): Promise<Rect> {
  const box = await locator.boundingBox({ timeout: 10_000 });
  expect(box, `${what} must be laid out`).not.toBeNull();
  expect(box!.width, `${what} must have a width`).toBeGreaterThan(0);
  expect(box!.height, `${what} must have a height`).toBeGreaterThan(0);
  return { left: box!.x, top: box!.y, right: box!.x + box!.width, bottom: box!.y + box!.height };
}

/** Half-open on both axes, so boxes that merely touch do not count. */
function intersects(a: Rect, b: Rect): boolean {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

function expectClose(actual: number, expected: number, what: string): void {
  expect(
    Math.abs(actual - expected),
    `${what}: got ${actual}, want ${expected}`,
  ).toBeLessThanOrEqual(EPS);
}

type ClipboardMode = "ok" | "reject" | "absent";

interface ClipboardStub {
  __clipboardMode?: ClipboardMode;
  __clipboardWrites?: string[];
}

// `meeting_footer.rs::clipboard()` does a `Reflect::get` per click, so an own
// accessor on `navigator` shadows the native one and `__clipboardMode` can be
// flipped between clicks without a reload.
function installClipboardStub(): void {
  const w = window as unknown as ClipboardStub;
  w.__clipboardMode = "ok";
  w.__clipboardWrites = [];
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    get() {
      if (w.__clipboardMode === "absent") {
        return undefined;
      }
      return {
        writeText(text: string) {
          w.__clipboardWrites!.push(text);
          return w.__clipboardMode === "reject"
            ? Promise.reject(new Error("e2e: clipboard write denied"))
            : Promise.resolve();
        },
      };
    },
  });
}

async function newMeetingPage(
  browser: Browser,
  uiURL: string,
  who: string,
  opts: {
    storage?: Record<string, string>;
    gitInfo?: "true" | "false";
    clipboardStub?: boolean;
  } = {},
): Promise<Page> {
  const ctx = await createAuthenticatedContext(browser, `${who}@videocall.rs`, who, uiURL);
  await ctx.addInitScript((entries: Record<string, string>) => {
    try {
      for (const [key, value] of Object.entries(entries)) {
        localStorage.setItem(key, value);
      }
    } catch {
      /* opaque-origin documents have no storage; the app origin does */
    }
  }, opts.storage ?? {});
  if (opts.gitInfo) {
    await setShowBuildGitInfoFlag(ctx, opts.gitInfo);
  }
  if (opts.clipboardStub) {
    await ctx.addInitScript(installClipboardStub);
  }
  const page = await ctx.newPage();
  await page.setViewportSize(DESKTOP);
  return page;
}

async function setClipboardMode(page: Page, mode: ClipboardMode): Promise<void> {
  await page.evaluate((next: ClipboardMode) => {
    const w = window as unknown as ClipboardStub;
    if (w.__clipboardMode === undefined) {
      throw new Error("the clipboard stub is not installed on this page");
    }
    w.__clipboardMode = next;
  }, mode);
}

async function clipboardWrites(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const w = window as unknown as ClipboardStub;
    if (!w.__clipboardWrites) {
      throw new Error("the clipboard stub is not installed on this page");
    }
    return w.__clipboardWrites;
  });
}

interface CopyFeedback {
  button: string;
  status: string;
}

// The button and the status region both revert after `COPY_RESET_MS` (1.6s), so
// a live `toHaveText` races that timer. This records every real DOM state they
// pass through instead of sampling one.
async function recordCopyFeedback(page: Page): Promise<void> {
  await page.evaluate(
    ({ dialogId, copyTestid }) => {
      const dialog = document.getElementById(dialogId);
      if (!dialog) {
        throw new Error(`#${dialogId} is not in the DOM`);
      }
      if (!dialog.querySelector(`[data-testid="${copyTestid}"]`)) {
        throw new Error("the Copy link button is not in the dialog");
      }
      if (!dialog.querySelector('[role="status"]')) {
        throw new Error('the dialog has no role="status" region');
      }
      const w = window as unknown as {
        __copyFeedback: { button: string; status: string }[];
        __copyObserver?: MutationObserver;
      };
      w.__copyObserver?.disconnect();
      w.__copyFeedback = [];
      const snap = () => {
        const button = dialog.querySelector(`[data-testid="${copyTestid}"]`);
        const status = dialog.querySelector('[role="status"]');
        w.__copyFeedback.push({
          button: (button?.textContent ?? "").trim(),
          status: (status?.textContent ?? "").trim(),
        });
      };
      snap();
      w.__copyObserver = new MutationObserver(snap);
      w.__copyObserver.observe(dialog, {
        subtree: true,
        childList: true,
        characterData: true,
      });
    },
    { dialogId: DIALOG.slice(1), copyTestid: COPY_TESTID },
  );
}

async function copyFeedback(page: Page): Promise<CopyFeedback[]> {
  return page.evaluate(() => {
    const w = window as unknown as { __copyFeedback?: CopyFeedback[] };
    if (!w.__copyFeedback) {
      throw new Error("recordCopyFeedback has not run on this page");
    }
    return w.__copyFeedback;
  });
}

async function expectCopyFeedback(page: Page, expected: CopyFeedback, when: string): Promise<void> {
  await expect
    .poll(async () => (await copyFeedback(page)).map((seen) => seen.button), {
      timeout: 8_000,
      message: `${when}: the Copy button must read "${expected.button}"`,
    })
    .toContain(expected.button);
  await expect
    .poll(async () => (await copyFeedback(page)).map((seen) => seen.status), {
      timeout: 8_000,
      message: `${when}: the hidden status region must announce "${expected.status}"`,
    })
    .toContain(expected.status);
}

async function joinSoloMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, username);
  await page.waitForTimeout(1000);
  expect(await joinMeetingFromPage(page)).toBe("in-meeting");
  await expect(page.locator(GRID)).toBeVisible({ timeout: 15_000 });
}

async function expectFooter(page: Page): Promise<Locator> {
  const footer = page.locator(FOOTER);
  await expect(footer).toBeVisible({ timeout: 15_000 });
  return footer;
}

async function wakeBar(page: Page): Promise<void> {
  await page.mouse.move(Math.floor(DESKTOP.width / 2), Math.floor(DESKTOP.height / 2));
  await page.waitForTimeout(300);
}

async function addMockPeers(page: Page, count: number): Promise<void> {
  await wakeBar(page);
  const mockButton = page.locator("button.video-control-button", {
    has: page.locator(".tooltip", { hasText: /Mock Peers/i }),
  });
  await expect(
    mockButton,
    "Mock Peers must be present (MOCK_PEERS_ENABLED=true in docker/docker-compose.e2e.yaml)",
  ).toBeVisible({ timeout: 10_000 });
  await mockButton.click({ timeout: 10_000 });
  const countInput = page.locator(".mock-peers-popover input[type='number']");
  await expect(countInput).toBeVisible({ timeout: 5_000 });
  await countInput.fill(String(count));
  await page.waitForTimeout(400);
  await page.locator(".mock-peers-popover-close").click({ timeout: 5_000 });
  await expect(page.locator(".mock-peers-popover")).not.toBeVisible({ timeout: 5_000 });
  await expect(page.locator(GRID_TILES)).toHaveCount(count, { timeout: 15_000 });
}

/** Wakes the dock, parks the pointer on its settled box so `:hover` holds it docked. */
async function holdDock(page: Page): Promise<Rect> {
  const dock = page.locator(DOCK);
  await expect(dock).toHaveCount(1);
  await wakeBar(page);
  await dock.hover({ timeout: 10_000 });
  await expect
    .poll(() => dock.evaluate((el) => el.matches(":hover")), {
      timeout: 5_000,
      message: "the pointer must be holding the dock",
    })
    .toBe(true);
  await page.waitForTimeout(700); // the dock's transform transition is 0.55s
  return rectOf(dock, "the dock");
}

async function openDialog(page: Page): Promise<Locator> {
  await page.locator(TRIGGER).click({ timeout: 10_000 });
  const dialog = page.locator(DIALOG);
  await expect(dialog).toBeVisible({ timeout: 5_000 });
  return dialog;
}

async function expectDialogClosedWithFocusOnTrigger(page: Page, how: string): Promise<void> {
  await expect(page.locator(BACKDROP), `${how} must close the dialog`).toHaveCount(0, {
    timeout: 5_000,
  });
  await expect
    .poll(() => page.evaluate(() => document.activeElement?.id), {
      timeout: 5_000,
      message: `${how} must return focus to #${TRIGGER_ID}`,
    })
    .toBe(TRIGGER_ID);
}

test.describe("In-call meeting footer and Meeting info dialog (issue 2791)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the footer opens Meeting info and Escape hands focus back to it @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterCore");
      const meetingId = `e2e_footer_core_${Date.now()}`;
      await joinSoloMeeting(page, meetingId, "FooterCore");

      await expectFooter(page);
      await expect(page.locator(`#main-container > ${FOOTER}`)).toHaveCount(1);
      const trigger = page.getByRole("button", { name: /^Meeting info/ });
      await expect(trigger).toHaveAttribute("id", TRIGGER_ID);
      await expect(trigger).toHaveAttribute(
        "aria-label",
        `Meeting info, meeting ID ${meetingId}, videocall-ui version ${CLIENT_VERSION}`,
      );

      await trigger.click({ timeout: 10_000 });
      const dialog = page.locator(DIALOG);
      await expect(dialog).toBeVisible({ timeout: 5_000 });
      await expect(dialog).toHaveAttribute("role", "dialog");
      await expect(dialog, "the dialog takes focus so Escape reaches it").toBeFocused({
        timeout: 5_000,
      });
      await expect(dialog.locator(MEETING_ID_VALUE)).toHaveText(meetingId);
      await expect(page.locator(`#main-container ${DIALOG}`)).toHaveCount(0);

      await page.keyboard.press("Escape");
      await expectDialogClosedWithFocusOnTrigger(page, "Escape");
    } finally {
      await browser.close();
    }
  });

  test("the footer line shows a ticking timer, the meeting ID, the head count and the version", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterLine", {
        gitInfo: "true",
      });
      const meetingId = `e2e_footer_line_${Date.now()}`;
      await joinSoloMeeting(page, meetingId, "FooterLine");
      const footer = await expectFooter(page);

      const timer = footer.locator('[data-testid="meeting-footer-timer"]');
      await expect(timer, "MEETING_STARTED must start the timer").toHaveText(TIMER_RE, {
        timeout: 30_000,
      });
      const first = (await timer.textContent())?.trim();
      expect(first).toMatch(TIMER_RE);
      await expect(timer, "the timer must tick").not.toHaveText(first!, { timeout: 5_000 });
      await expect(timer).toHaveText(TIMER_RE);

      await expect(footer.locator('[data-testid="meeting-footer-meeting-id"]')).toContainText(
        meetingId,
      );
      await expect(footer.locator('[data-testid="meeting-footer-version"]')).toHaveText(
        `v${CLIENT_VERSION}`,
      );
      await expect(
        footer.locator('[data-testid="meeting-footer-participants"] .meeting-footer-count-text'),
        "the count includes the local participant",
      ).toHaveText("1 participant");

      const dialog = await openDialog(page);
      const row = (name: string) => dialog.locator(`[data-testid="meeting-info-row-${name}"]`);
      await expect(dialog.locator(MEETING_ID_VALUE)).toHaveText(meetingId);
      await expect(row("link").locator(".meeting-info-link")).toHaveText(
        new RegExp(`/meeting/${meetingId}$`),
      );
      await expect(row("duration").locator(".about-modal-value")).toHaveText(TIMER_RE);
      await expect(row("participants").locator(".about-modal-value")).toHaveText("1");
      await expect(row("version").locator(".about-modal-value")).toHaveText(
        `videocall-ui v${CLIENT_VERSION}`,
      );
      await expect(row("commit"), "showBuildGitInfo is on for this context").toHaveCount(1);
      await expect(row("branch")).toHaveCount(1);
    } finally {
      await browser.close();
    }
  });

  test("the footer is a strip on the viewport bottom that the dock and every tile clear", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterGeo", {
        storage: { vc_self_view_placement: "corner" },
      });
      await joinSoloMeeting(page, `e2e_footer_geo_${Date.now()}`, "FooterGeo");
      await addMockPeers(page, 4);
      await expect(page.locator(DOCK)).toHaveClass(/\bdock-bottom\b/);

      const dock = await holdDock(page);
      const footer = await rectOf(await expectFooter(page), "the meeting footer");
      expectClose(footer.bottom - footer.top, FOOTER_H, "footer height");
      expectClose(footer.bottom, DESKTOP.height, "footer bottom edge");
      expectClose(footer.left, 0, "footer left edge with no drawer open");
      expectClose(footer.right, DESKTOP.width, "footer right edge with no drawer open");
      expect(dock.bottom, "the dock must sit above the footer").toBeLessThanOrEqual(footer.top);

      const tiles = await page.locator(GRID_TILES).all();
      expect(tiles.length).toBe(4);
      for (const [i, tile] of tiles.entries()) {
        const r = await rectOf(tile, `tile ${i + 1}`);
        expect(intersects(r, footer), `tile ${i + 1} must not reach the footer`).toBe(false);
        expect(intersects(r, dock), `tile ${i + 1} (bottom ${r.bottom}) must clear the dock`).toBe(
          false,
        );
      }
    } finally {
      await browser.close();
    }
  });

  test("with a left dock the dock and every tile stay above the footer", async ({ baseURL }) => {
    test.setTimeout(150_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterLeft", {
        storage: { vc_dock_position: "left", vc_self_view_placement: "corner" },
      });
      await joinSoloMeeting(page, `e2e_footer_left_${Date.now()}`, "FooterLeft");
      await expect(page.locator(DOCK)).toHaveClass(/\bdock-left\b/, { timeout: 10_000 });
      await addMockPeers(page, 4);

      const dock = await holdDock(page);
      const footer = await rectOf(await expectFooter(page), "the meeting footer");
      expectClose(footer.bottom - footer.top, FOOTER_H, "footer height");
      expectClose(footer.bottom, DESKTOP.height, "footer bottom edge");
      expect(dock.bottom, "the side dock must end above the footer").toBeLessThanOrEqual(
        footer.top,
      );

      const tiles = await page.locator(GRID_TILES).all();
      expect(tiles.length).toBe(4);
      for (const [i, tile] of tiles.entries()) {
        const r = await rectOf(tile, `tile ${i + 1}`);
        expect(
          intersects(r, footer),
          `tile ${i + 1} (bottom ${r.bottom}) must end above the footer (top ${footer.top})`,
        ).toBe(false);
      }
    } finally {
      await browser.close();
    }
  });

  test("Close and a backdrop click return focus to the footer; a drag out of the card does not close it", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterClose");
      await joinSoloMeeting(page, `e2e_footer_close_${Date.now()}`, "FooterClose");
      await expectFooter(page);

      let dialog = await openDialog(page);
      await dialog.locator('[data-testid="meeting-info-dialog-close"]').click({ timeout: 5_000 });
      await expectDialogClosedWithFocusOnTrigger(page, "Close");

      dialog = await openDialog(page);
      const outside = { x: 8, y: 8 };
      expect(
        await page.evaluate(
          ({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null,
          outside,
        ),
        "the release point must be on the backdrop, outside the card",
      ).toBe(BACKDROP_TESTID);
      await page.evaluate((testid) => {
        const backdrop = document.querySelector(`[data-testid="${testid}"]`);
        if (!backdrop) throw new Error("the dialog backdrop is not in the DOM");
        const w = window as unknown as { __backdropClicks: number };
        w.__backdropClicks = 0;
        backdrop.addEventListener("click", () => {
          w.__backdropClicks += 1;
        });
      }, BACKDROP_TESTID);

      const value = await rectOf(dialog.locator(MEETING_ID_VALUE), "the meeting ID value");
      await page.mouse.move((value.left + value.right) / 2, (value.top + value.bottom) / 2);
      await page.mouse.down();
      await page.mouse.move(outside.x, outside.y, { steps: 8 });
      await page.mouse.up();
      await expect
        .poll(
          () =>
            page.evaluate(
              () => (window as unknown as { __backdropClicks: number }).__backdropClicks,
            ),
          { timeout: 5_000, message: "the drag's release must click the backdrop" },
        )
        .toBe(1);
      await page.waitForTimeout(500); // window for an unwanted close to render
      await expect(dialog, "a press that began inside the card must not close it").toBeVisible();

      await page.locator(BACKDROP).click({ position: outside, timeout: 5_000 });
      await expectDialogClosedWithFocusOnTrigger(page, "A backdrop click");
    } finally {
      await browser.close();
    }
  });

  test("an open peer list survives the footer and its dialog, and has no More options menu", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterList");
      await joinSoloMeeting(page, `e2e_footer_list_${Date.now()}`, "FooterList");
      await openPeerList(page);

      const peerList = page.locator(PEER_LIST);
      const header = peerList.locator(".sidebar-header");
      await expect(header.locator("h2")).toHaveText("Attendants");
      await expect(header.locator(".header-actions .close-button")).toHaveCount(1);
      await expect(header.locator('[aria-label="More options"]')).toHaveCount(0);

      await expectFooter(page);
      const list = await rectOf(peerList, "the peer list");
      await expect
        .poll(
          async () => {
            const b = await page.locator(FOOTER).boundingBox();
            return b ? `${Math.round(b.x)}..${Math.round(b.x + b.width)}` : "missing";
          },
          { timeout: 10_000, message: "the footer must span the band right of the peer list" },
        )
        .toBe(`${Math.round(list.right)}..${DESKTOP.width}`);

      const dialog = await openDialog(page);
      await expect(peerList, "a footer click must not light-dismiss the peer list").toHaveClass(
        /\bvisible\b/,
        { timeout: 2_000 },
      );
      await expect(dialog).toBeFocused({ timeout: 5_000 });
      await page.keyboard.press("Escape");
      await expectDialogClosedWithFocusOnTrigger(page, "Escape");
      await expect(peerList, "Escape in the dialog must close only the dialog").toHaveClass(
        /\bvisible\b/,
        { timeout: 2_000 },
      );
    } finally {
      await browser.close();
    }
  });

  test("Copy link reports a rejected write and a missing clipboard on the button itself", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const page = await newMeetingPage(browser, baseURL || DEFAULT_UI_URL, "FooterCopy", {
        clipboardStub: true,
      });
      const meetingId = `e2e_footer_copy_${Date.now()}`;
      await joinSoloMeeting(page, meetingId, "FooterCopy");
      await expectFooter(page);

      const dialog = await openDialog(page);
      const copyButton = dialog.locator(COPY_BUTTON);
      await expect(copyButton, "the button starts idle").toHaveText("Copy link");
      const link = dialog.locator(".meeting-info-link");
      await expect(link).toHaveText(new RegExp(`/meeting/${meetingId}$`));
      const linkText = ((await link.textContent()) ?? "").trim();

      // Premise: a clipboard that ACCEPTS the write reports success, so the two
      // failure arms cannot pass on a button that always says "Copy failed".
      await recordCopyFeedback(page);
      await copyButton.click({ timeout: 5_000 });
      await expectCopyFeedback(
        page,
        { button: "Copied", status: "Meeting link copied" },
        "a clipboard that accepts the write",
      );
      expect(
        await clipboardWrites(page),
        "the button writes the meeting link the dialog shows",
      ).toEqual([linkText]);
      await expect(copyButton, "the label returns to idle").toHaveText("Copy link", {
        timeout: 8_000,
      });

      // A clipboard that rejects the write.
      await setClipboardMode(page, "reject");
      await recordCopyFeedback(page);
      await copyButton.click({ timeout: 5_000 });
      await expectCopyFeedback(
        page,
        { button: "Copy failed", status: COPY_FAILED_ANNOUNCEMENT },
        "a rejected clipboard write",
      );
      expect((await clipboardWrites(page)).length, "the rejected click did reach writeText").toBe(
        2,
      );
      await expect(copyButton, "the label returns to idle").toHaveText("Copy link", {
        timeout: 8_000,
      });

      // No `navigator.clipboard` at all — the other arm of `clipboard()`.
      await setClipboardMode(page, "absent");
      await recordCopyFeedback(page);
      await copyButton.click({ timeout: 5_000 });
      await expectCopyFeedback(
        page,
        { button: "Copy failed", status: COPY_FAILED_ANNOUNCEMENT },
        "a missing navigator.clipboard",
      );
      expect(
        (await clipboardWrites(page)).length,
        "with no clipboard there is nothing to write to",
      ).toBe(2);
    } finally {
      await browser.close();
    }
  });
});
