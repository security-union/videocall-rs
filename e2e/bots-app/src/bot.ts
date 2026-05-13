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

  return { browser, context, page, shutdown };
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
