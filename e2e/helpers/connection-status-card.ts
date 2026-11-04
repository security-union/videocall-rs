import { expect, BrowserContext, Page } from "@playwright/test";

// Copied from dioxus-ui/src/components/connection_status.rs.
export const CARD_TITLE = {
  preferred: "Connecting to the meeting…",
  fallback: "Trying another connection…",
  connectedWs: "Connected using WebSocket",
  connectedWt: "Connected using WebTransport",
};

export const CARD_SELECTOR = '[data-testid="connection-status-card"]';

export interface CardSnapshot {
  at: number;
  phase: string | null;
  title: string | null;
  detail: string | null;
  countdown: boolean;
  announce: string | null;
}

// One entry per change of what the card shows.
const CARD_RECORDER = `(() => {
  const log = [];
  window.__e2eConnectionCard = log;
  let last = "";
  const read = (id) => {
    const el = document.querySelector('[data-testid="' + id + '"]');
    return el ? el.textContent : null;
  };
  const snap = () => {
    const card = document.querySelector('${CARD_SELECTOR}');
    const announce = read("connection-status-announcer");
    const s = card
      ? {
          announce,
          phase: card.getAttribute("data-phase"),
          title: read("connection-status-title"),
          detail: read("connection-status-detail"),
          countdown: read("connection-status-countdown") !== null,
        }
      : { announce, phase: null, title: null, detail: null, countdown: false };
    const key = JSON.stringify(s);
    if (key !== last) {
      last = key;
      log.push({ at: performance.now(), ...s });
    }
  };
  new MutationObserver(snap).observe(document, {
    childList: true,
    subtree: true,
    attributes: true,
    characterData: true,
  });
})();`;

/** Call before the first navigation; it re-installs on every document. */
export async function installConnectionCardRecorder(context: BrowserContext): Promise<void> {
  await context.addInitScript(CARD_RECORDER);
}

export async function cardLog(page: Page): Promise<CardSnapshot[]> {
  const log = await page.evaluate(
    () =>
      (window as unknown as { __e2eConnectionCard?: CardSnapshot[] }).__e2eConnectionCard ?? null,
  );
  expect(log, "the card recorder must be installed").not.toBeNull();
  return log as CardSnapshot[];
}

/** Each distinct live-region message in order; a trailing NBSP re-triggers one. */
export function announcements(log: CardSnapshot[]): string[] {
  return log
    .map((s) => (s.announce ?? "").replace(/\u00a0$/, ""))
    .filter((a, i, all) => a !== "" && (i === 0 || all[i - 1] !== a));
}

export function phases(log: CardSnapshot[]): string[] {
  return log
    .map((s) => s.phase)
    .filter((p): p is string => p !== null)
    .filter((p, i, all) => i === 0 || all[i - 1] !== p);
}

/** Waits for `connectedTitle` after entry `from`, then for the card to leave. */
export async function settledCardLog(
  page: Page,
  connectedTitle: string,
  from = 0,
): Promise<CardSnapshot[]> {
  await expect
    .poll(async () => (await cardLog(page)).slice(from).some((s) => s.title === connectedTitle), {
      timeout: 30_000,
    })
    .toBe(true);
  await expect(page.locator(CARD_SELECTOR)).toHaveCount(0, { timeout: 10_000 });
  return (await cardLog(page)).slice(from);
}
