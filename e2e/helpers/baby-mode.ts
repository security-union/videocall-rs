import { Locator, Page, expect } from "@playwright/test";
import { clickBarControl } from "./meeting";

/** Matches `HOLD_TO_UNLOCK_MS` in dioxus-ui/src/components/baby_mode.rs. */
export const HOLD_TO_UNLOCK_MS = 3_000;

/** Matches `UNLOCK_CODE_LEN` in dioxus-ui/src/components/baby_mode.rs. */
export const UNLOCK_CODE_LEN = 4;

/** Matches `UNLOCK_DIALOG_IDLE_MS` in dioxus-ui/src/components/baby_mode.rs. */
export const UNLOCK_DIALOG_IDLE_MS = 15_000;

/** sessionStorage key the UI uses to keep baby mode on across reloads of the tab. */
export const BABY_MODE_STORAGE_KEY = "vc_baby_mode";

/** Status text shown under the lock for a few seconds after locking. */
export const ENTRY_HINT = "Controls locked. Hold the lock for 3 seconds to unlock.";

/** Status text shown under the lock after a plain tap. */
export const TAP_HINT = "Hold to unlock";

/** Feedback shown in the dialog after a wrong code. */
export const WRONG_CODE_FEEDBACK = "That didn't match. Here's a new code.";

/** Accessible name of the control-bar button that turns baby mode on. */
export const ENABLE_BUTTON_NAME = "Lock controls (baby mode)";

export function babyModeButton(page: Page): Locator {
  return page.getByTestId("baby-mode-button");
}

export function babyModeShield(page: Page): Locator {
  return page.getByTestId("baby-mode-shield");
}

export function babyModeLock(page: Page): Locator {
  return page.getByTestId("baby-mode-lock");
}

export function unlockDialog(page: Page): Locator {
  return page.getByTestId("baby-mode-unlock-dialog");
}

export function unlockCode(page: Page): Locator {
  return page.getByTestId("baby-mode-code");
}

export function unlockCodeInput(page: Page): Locator {
  return page.getByTestId("baby-mode-code-input");
}

export function unlockSubmit(page: Page): Locator {
  return page.getByTestId("baby-mode-unlock-submit");
}

export function unlockCancel(page: Page): Locator {
  return page.getByTestId("baby-mode-unlock-cancel");
}

export function babyModeHint(page: Page): Locator {
  return page.getByTestId("baby-mode-hint");
}

export function unlockFeedback(page: Page): Locator {
  return page.getByTestId("baby-mode-feedback");
}

/** Read the persisted baby mode flag straight from sessionStorage. */
export async function storedBabyMode(page: Page): Promise<string | null> {
  return page.evaluate((key) => window.sessionStorage.getItem(key), BABY_MODE_STORAGE_KEY);
}

/**
 * Click the control-bar button (on a phone, the More menu's row) and wait
 * for the locked state.
 */
export async function enableBabyMode(page: Page): Promise<void> {
  await clickBarControl(page, ENABLE_BUTTON_NAME);
  await expect(babyModeLock(page)).toBeVisible();
  await expect(babyModeShield(page)).toBeVisible();
}

/**
 * Press the primary mouse button on the lock and keep it down for `holdMs`
 * before releasing. The lock reacts to pointer events, which Playwright's
 * mouse API dispatches as real input. The wait is the behaviour under test,
 * so a fixed timeout is intended here.
 */
export async function holdLock(page: Page, holdMs: number): Promise<void> {
  await pressLock(page);
  await page.waitForTimeout(holdMs);
  await page.mouse.up();
}

/** Move to the lock's centre and press the primary mouse button (no release). */
export async function pressLock(page: Page): Promise<void> {
  const box = await babyModeLock(page).boundingBox();
  if (!box) {
    throw new Error("baby mode lock has no bounding box");
  }
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
}

/** Hold the lock until the unlock dialog opens, then release. Returns the code. */
export async function openUnlockDialog(page: Page): Promise<string> {
  await pressLock(page);
  await expect(unlockDialog(page)).toBeVisible({ timeout: HOLD_TO_UNLOCK_MS + 5_000 });
  await page.mouse.up();
  const code = (await unlockCode(page).textContent())?.trim() ?? "";
  expect(code).toHaveLength(UNLOCK_CODE_LEN);
  return code;
}

/**
 * Focus the lock and hold `key` down for `holdMs` before releasing. Playwright's
 * `keyboard.down` sends one keydown with no auto-repeat, which is how the lock
 * sees a held key.
 */
export async function holdLockWithKey(page: Page, key: string, holdMs: number): Promise<void> {
  await babyModeLock(page).focus();
  await page.keyboard.down(key);
  await page.waitForTimeout(holdMs);
  await page.keyboard.up(key);
}
