/** The local recording status UI of `dioxus-ui/src/components/recording_status_bar.rs`. */

import { Locator, Page, expect } from "@playwright/test";

export const STATUS_BAR = ".meeting-status-bar";

const REC_AREA = ".meeting-status-rec-slot > .meeting-status-rec-area";

const STATUS_ITEM = `${STATUS_BAR} > ${REC_AREA} > .meeting-status-item.meeting-status-item--recording`;

/** The recorder's own item (issue 3126): "You are recording" plus the file name. */
export const SELF_STATUS_ITEM = `${STATUS_ITEM}.meeting-status-item--self`;

export const PEER_STATUS_ITEM = `${STATUS_ITEM}:not(.meeting-status-item--self)`;

/** While Activating, the bar is pre-mounted as a hidden, empty live region. */
export const PENDING_STATUS_ITEM = `.visually-hidden > ${REC_AREA} > .meeting-status-item.visually-hidden[role="status"]`;

/** "Starting recording…", shown only while Activating. */
export const STARTING_TOAST = ".peer-toasts > .peer-toast.recording-status-banner";

export const SAVED_TOAST = ".peer-toasts > .peer-toast.recording-saved-banner";

export const SELF_RECORDING_LABEL = /^You (and \d+ others? )?are recording$/;

export async function expectSelfRecording(
  page: Page,
  label: string | RegExp = SELF_RECORDING_LABEL,
): Promise<void> {
  await expect(
    page.locator(SELF_STATUS_ITEM).locator(":scope > .meeting-status-item-label"),
  ).toHaveText(label, { timeout: 15_000 });
}

/** The width the self item's `@container rec-slot` queries see. */
export async function expectRecSlotInlineSize(page: Page, min: number, max: number): Promise<void> {
  const item = page.locator(SELF_STATUS_ITEM);
  await expect(item).toBeVisible();
  await expect
    .poll(
      async () => {
        const size = await item.evaluate((el) => {
          let c = el.parentElement;
          const named = (e: Element) =>
            getComputedStyle(e).getPropertyValue("container-name").split(" ").includes("rec-slot");
          while (c && !named(c)) c = c.parentElement;
          if (!c) return "no rec-slot container";
          const cs = getComputedStyle(c);
          const edges = [cs.paddingLeft, cs.paddingRight, cs.borderLeftWidth, cs.borderRightWidth];
          return c.getBoundingClientRect().width - edges.reduce((s, v) => s + parseFloat(v), 0);
        });
        return typeof size === "number" && size >= min && size <= max ? "in range" : `${size}`;
      },
      { message: `rec-slot inline size in [${min}, ${max}]`, timeout: 10_000 },
    )
    .toBe("in range");
}

export async function expectSelfItemContained(page: Page): Promise<void> {
  const item = page.locator(SELF_STATUS_ITEM);
  await expect(item.locator(":scope > .rec-dot")).toBeVisible();
  await expect(item.locator(":scope > .meeting-status-item-label")).toBeVisible();
  await expect
    .poll(() =>
      item.evaluate((el) => {
        const right = el.getBoundingClientRect().right;
        const over = Array.from(el.children)
          .filter((c) => !c.classList.contains("visually-hidden") && c.getClientRects().length > 0)
          .filter((c) => c.getBoundingClientRect().right > right + 1)
          .map((c) => `.${c.className} ends at ${c.getBoundingClientRect().right} > ${right}`);
        if (el.scrollWidth > el.clientWidth + 1) {
          over.push(`scrollWidth ${el.scrollWidth} > clientWidth ${el.clientWidth}`);
        }
        const bar = el.closest(".meeting-status-bar")!.getBoundingClientRect();
        if (right > bar.right + 1) over.push(`item ends at ${right} > bar ${bar.right}`);
        return over;
      }),
    )
    .toEqual([]);
}

/** The file name a `.rec-target` renders, rejoined from its head + tail spans. */
export async function fileNameIn(target: Locator): Promise<string> {
  const parts = target.locator(":scope > :is(.rec-target-head, .rec-target-tail)");
  await expect(parts).toHaveCount(2);
  return (await parts.allTextContents()).join("");
}

export async function expectSavedToastNames(page: Page, name: string): Promise<void> {
  const toast = page.locator(SAVED_TOAST);
  const text = toast.locator(":scope > .toast-text");
  await expect(text.locator(":scope > .toast-name")).toHaveText("Recording saved", {
    timeout: 20_000,
  });
  await expect(text.locator(":scope > .visually-hidden")).toHaveText(` as ${name}`);
  await expect(toast).not.toHaveAttribute("aria-label");
  expect(await fileNameIn(text.locator(":scope > .rec-target"))).toBe(name);
}
