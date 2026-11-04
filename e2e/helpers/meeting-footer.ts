import { Page, expect } from "@playwright/test";

export const MEETING_FOOTER_TRIGGER = "#meeting-footer-trigger";
export const MEETING_FOOTER_LINE = '[data-testid="meeting-footer"] > .meeting-footer-content';
export const MEETING_FOOTER_VERSION = `${MEETING_FOOTER_LINE} > .meeting-footer-group--app [data-testid="meeting-footer-version"]`;

/**
 * Clicks the Meeting info trigger at the centre of `label`, a part of the line
 * over it, so the click cannot land on the participant count (issue 3122).
 */
export async function clickMeetingInfoTriggerAt(
  page: Page,
  label: string = MEETING_FOOTER_VERSION,
): Promise<void> {
  const trigger = page.locator(MEETING_FOOTER_TRIGGER);
  const part = page.locator(label);
  await expect(trigger).toBeVisible({ timeout: 15_000 });
  await expect(part, `${label} must be shown to click through it`).toBeVisible({
    timeout: 5_000,
  });
  const t = await trigger.boundingBox();
  const p = await part.boundingBox();
  expect(t, "the Meeting info trigger must be laid out").not.toBeNull();
  expect(p, `${label} must be laid out`).not.toBeNull();
  await trigger.click({
    position: { x: p!.x + p!.width / 2 - t!.x, y: p!.y + p!.height / 2 - t!.y },
    timeout: 10_000,
  });
}
