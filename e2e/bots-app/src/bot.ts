import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-dev-shm-usage",
];

export interface BotRunOptions {
  meetingURL: string;
  participant: string;
  displayName: string;
  headless: boolean;
}

export interface BotHandle {
  browser: Browser;
  context: BrowserContext;
  page: Page;
  /**
   * Best-effort click the meeting's HangUp button and wait briefly for the
   * client-side leave-meeting API call to settle. Idempotent: if the button
   * is not visible (the bot never finished joining), this returns without
   * raising. Always followed by `shutdown` for the actual browser teardown.
   */
  leaveMeeting: () => Promise<void>;
  shutdown: () => Promise<void>;
}

export async function launchBot(opts: BotRunOptions): Promise<BotHandle> {
  const target = new URL(opts.meetingURL);
  const baseURL = `${target.protocol}//${target.host}`;
  const email = participantEmail(opts.participant);

  const browser = await chromium.launch({
    headless: opts.headless,
    args: CHROME_ARGS,
  });
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  await applyJwtCookieAuth(context, {
    email,
    displayName: opts.displayName,
    baseURL,
  });

  const page = await context.newPage();
  page.on("pageerror", (err) => {
    console.error(`[${opts.participant}] pageerror:`, err.message);
  });
  page.on("console", (msg) => {
    if (msg.type() === "error") {
      console.error(`[${opts.participant}] console.error:`, msg.text());
    }
  });

  console.log(`[${opts.participant}] navigating to ${opts.meetingURL}`);
  await page.goto(opts.meetingURL, { waitUntil: "domcontentloaded" });

  const leaveMeeting = async (): Promise<void> => {
    const hangUp = page.locator("button.video-control-button", {
      has: page.locator("span.tooltip", { hasText: "Hang Up" }),
    });
    try {
      if (await hangUp.isVisible({ timeout: 1_000 }).catch(() => false)) {
        await hangUp.click({ timeout: 2_000 });
        // After hang-up the page navigates to `/`. Wait briefly so the
        // client-side `meeting_api::leave_meeting` request has time to
        // reach the server before we tear the context down.
        await page
          .waitForURL((url) => url.pathname === "/", { timeout: 2_000 })
          .catch(() => {
            // Falling through is fine — the API call may still complete
            // after we close, and the relay handles the disconnect anyway.
          });
      }
    } catch (e) {
      console.error(`[${opts.participant}] leaveMeeting failed:`, e);
    }
  };

  const shutdown = async (): Promise<void> => {
    try {
      await context.close();
    } catch (e) {
      console.error(`[${opts.participant}] context.close failed:`, e);
    }
    try {
      await browser.close();
    } catch (e) {
      console.error(`[${opts.participant}] browser.close failed:`, e);
    }
  };

  return { browser, context, page, leaveMeeting, shutdown };
}

/**
 * Maps a participant handle (e.g. "alice") to an email used as the JWT
 * subject. Mirrors the manifest convention used by `bot/conversation/`.
 *
 * Override with a literal email if the participant string already contains
 * an "@".
 */
function participantEmail(participant: string): string {
  if (participant.includes("@")) {
    return participant;
  }
  return `${participant}@bots-app.local`;
}
