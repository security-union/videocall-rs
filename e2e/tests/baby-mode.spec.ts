import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { controlButton, joinMeetingFromPage, navigateToMeeting } from "../helpers/meeting";
import {
  ENABLE_BUTTON_NAME,
  ENTRY_HINT,
  HOLD_TO_UNLOCK_MS,
  TAP_HINT,
  UNLOCK_CODE_LEN,
  UNLOCK_DIALOG_IDLE_MS,
  WRONG_CODE_FEEDBACK,
  babyModeButton,
  babyModeHint,
  babyModeLock,
  babyModeShield,
  enableBabyMode,
  holdLock,
  holdLockWithKey,
  openUnlockDialog,
  pressLock,
  storedBabyMode,
  unlockCancel,
  unlockCode,
  unlockCodeInput,
  unlockDialog,
  unlockFeedback,
  unlockSubmit,
} from "../helpers/baby-mode";

// Long enough that a hold timer which was not cancelled would have fired.
const PAST_HOLD_MS = HOLD_TO_UNLOCK_MS + 1_000;

const CODE_PATTERN = new RegExp(`^\\S{${UNLOCK_CODE_LEN}}$`);

test.describe("Baby mode", () => {
  let meetingId: string;

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, page, baseURL }, testInfo) => {
    await injectSessionCookie(context, { baseURL });
    meetingId = `e2e_baby_${testInfo.workerIndex}_${Date.now()}`;
    await navigateToMeeting(page, meetingId, "baby-mode-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");
    await expect(controlButton(page, "Hang Up")).toBeVisible();
    await expect(babyModeButton(page)).toBeVisible();
  });

  test("the enable button is labelled for screen readers", async ({ page }) => {
    await expect(babyModeButton(page)).toHaveAccessibleName(ENABLE_BUTTON_NAME);
    await expect(page.getByRole("button", { name: ENABLE_BUTTON_NAME })).toBeVisible();
  });

  test("turning baby mode on hides the controls and shows the lock", async ({ page }) => {
    await enableBabyMode(page);

    await expect(page.locator("div.controls")).toHaveCount(0);
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
    await expect(babyModeButton(page)).toHaveCount(0);
    // The meeting itself stays on screen underneath the shield.
    await expect(page.locator("#grid-container")).toBeVisible();

    // The lock sits in the top-right corner.
    const box = await babyModeLock(page).boundingBox();
    const viewport = page.viewportSize();
    expect(box).not.toBeNull();
    expect(viewport).not.toBeNull();
    expect(box!.x + box!.width).toBeGreaterThan((viewport!.width * 3) / 4);
    expect(box!.y).toBeLessThan(viewport!.height / 4);

    // The entry hint explains how to get out.
    await expect(babyModeHint(page)).toHaveText(ENTRY_HINT);

    expect(await storedBabyMode(page)).toBe("1");
  });

  test("a quick tap on the lock shows the hold hint and does not open the dialog", async ({
    page,
  }) => {
    await enableBabyMode(page);

    await babyModeLock(page).click();
    await expect(babyModeHint(page)).toHaveText(TAP_HINT);

    await page.waitForTimeout(PAST_HOLD_MS);
    await expect(unlockDialog(page)).toHaveCount(0);
    await expect(babyModeLock(page)).toBeVisible();
    // The tap hint is shown for 2 s, so it has gone by now.
    await expect(babyModeHint(page)).toHaveText("");
  });

  test("releasing the lock before 3 seconds does not open the unlock dialog", async ({ page }) => {
    await enableBabyMode(page);

    await holdLock(page, HOLD_TO_UNLOCK_MS / 2);
    await expect(unlockDialog(page)).toHaveCount(0);

    // Wait past the point where an uncancelled hold timer would fire.
    await page.waitForTimeout(PAST_HOLD_MS);
    await expect(unlockDialog(page)).toHaveCount(0);
  });

  test("holding the lock for 3 seconds opens the unlock dialog", async ({ page }) => {
    await enableBabyMode(page);

    await pressLock(page);
    // Not open partway through the hold.
    await page.waitForTimeout(HOLD_TO_UNLOCK_MS / 2);
    await expect(unlockDialog(page)).toHaveCount(0);
    // Open once the hold completes, while the button is still down.
    await expect(unlockDialog(page)).toBeVisible({ timeout: HOLD_TO_UNLOCK_MS + 5_000 });
    await page.mouse.up();

    await expect(
      unlockDialog(page).getByRole("heading", { level: 2, name: "Unlock controls" }),
    ).toBeVisible();
    await expect(unlockCode(page)).toHaveText(CODE_PATTERN);
    // Screen readers get the code spelled out one character at a time.
    const code = (await unlockCode(page).textContent())!.trim();
    await expect(unlockCode(page)).toHaveAttribute("aria-label", code.split("").join(" "));
    await expect(unlockCodeInput(page)).toBeVisible();
    await expect(unlockCodeInput(page)).toBeFocused();
    await expect(unlockSubmit(page)).toBeVisible();
    await expect(unlockCancel(page)).toBeVisible();
  });

  test("holding Space on the focused lock opens the dialog; releasing early does not", async ({
    page,
  }) => {
    await enableBabyMode(page);

    // Released halfway: keyup cancels the hold.
    await holdLockWithKey(page, "Space", HOLD_TO_UNLOCK_MS / 2);
    await page.waitForTimeout(PAST_HOLD_MS);
    await expect(unlockDialog(page)).toHaveCount(0);

    // Held through: the dialog opens while the key is still down.
    await babyModeLock(page).focus();
    await page.keyboard.down("Space");
    await expect(unlockDialog(page)).toBeVisible({ timeout: HOLD_TO_UNLOCK_MS + 5_000 });
    await page.keyboard.up("Space");

    await expect(unlockCode(page)).toHaveText(CODE_PATTERN);
    await expect(unlockCodeInput(page)).toBeFocused();
  });

  test("Escape closes the unlock dialog and baby mode stays on", async ({ page }) => {
    await enableBabyMode(page);
    await openUnlockDialog(page);

    await unlockCodeInput(page).press("Escape");

    await expect(unlockDialog(page)).toHaveCount(0);
    await expect(babyModeLock(page)).toBeVisible();
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
  });

  test("Cancel closes the unlock dialog and returns focus to the lock", async ({ page }) => {
    await enableBabyMode(page);
    await openUnlockDialog(page);

    await unlockCancel(page).click();

    await expect(unlockDialog(page)).toHaveCount(0);
    await expect(babyModeLock(page)).toBeFocused();
    await expect(babyModeShield(page)).toBeVisible();
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
    expect(await storedBabyMode(page)).toBe("1");
  });

  test("the unlock dialog closes by itself after 15 seconds idle", async ({ page }) => {
    test.setTimeout(60_000);
    await enableBabyMode(page);
    await openUnlockDialog(page);
    await expect(unlockCodeInput(page)).toBeFocused();

    // Still open well before the idle limit.
    await page.waitForTimeout(UNLOCK_DIALOG_IDLE_MS / 2);
    await expect(unlockDialog(page)).toBeVisible();

    await expect(unlockDialog(page)).toHaveCount(0, { timeout: UNLOCK_DIALOG_IDLE_MS });
    await expect(babyModeLock(page)).toBeVisible();
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
  });

  test("a wrong code shows feedback, clears the input and shows a new code", async ({ page }) => {
    await enableBabyMode(page);
    const firstCode = await openUnlockDialog(page);
    await expect(unlockFeedback(page)).toHaveText("");

    // `0` is not in the unlock alphabet, so this can never be the code.
    await unlockCodeInput(page).fill("0000");
    await unlockSubmit(page).click();

    await expect(unlockFeedback(page)).toHaveText(WRONG_CODE_FEEDBACK);
    await expect(unlockCodeInput(page)).toHaveValue("");
    // A fresh random code; the chance it repeats is 1 in 31^4.
    await expect(unlockCode(page)).not.toHaveText(firstCode);
    await expect(unlockCode(page)).toHaveText(CODE_PATTERN);
    await expect(unlockDialog(page)).toBeVisible();
    await expect(babyModeLock(page)).toBeVisible();
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
  });

  test("the correct code in lowercase unlocks and Hang Up works again", async ({ page }) => {
    await enableBabyMode(page);
    const code = await openUnlockDialog(page);

    await unlockCodeInput(page).fill(code.toLowerCase());
    await unlockCodeInput(page).press("Enter");

    await expect(unlockDialog(page)).toHaveCount(0);
    await expect(babyModeShield(page)).toHaveCount(0);
    await expect(babyModeLock(page)).toHaveCount(0);
    await expect(page.locator("div.controls")).toBeVisible();
    expect(await storedBabyMode(page)).toBeNull();

    const hangUp = controlButton(page, "Hang Up");
    await expect(hangUp).toBeVisible();
    await hangUp.click();
    await expect(page).not.toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });
  });

  test("baby mode is still on after reloading the tab once back in the meeting", async ({
    page,
  }) => {
    await enableBabyMode(page);
    expect(await storedBabyMode(page)).toBe("1");

    // sessionStorage survives a reload of the same tab.
    await page.reload();
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");

    await expect(babyModeLock(page)).toBeVisible();
    await expect(babyModeShield(page)).toBeVisible();
    await expect(page.locator("div.controls")).toHaveCount(0);
    await expect(controlButton(page, "Hang Up")).toHaveCount(0);
  });

  test("clicking where Hang Up was while locked does not leave the meeting", async ({ page }) => {
    const hangUpBox = await controlButton(page, "Hang Up").boundingBox();
    expect(hangUpBox).not.toBeNull();

    await enableBabyMode(page);

    const x = hangUpBox!.x + hangUpBox!.width / 2;
    const y = hangUpBox!.y + hangUpBox!.height / 2;
    await page.mouse.click(x, y);
    await page.mouse.dblclick(x, y);

    // Hang Up navigates to "/" after an API call; give it time to (not) happen.
    await page.waitForTimeout(2_000);
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`));
    await expect(page.locator("#grid-container")).toBeVisible();
    await expect(babyModeLock(page)).toBeVisible();
  });
});
