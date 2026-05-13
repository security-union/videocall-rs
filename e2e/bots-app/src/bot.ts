import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";
import { type AuthBackend, requireStorageState } from "./auth/storage-state";
import { resolveAssetsForParticipant } from "./assets";
import { type Manifest } from "./manifest";

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
  /**
   * Auth backend selection. `"jwt"` injects a session cookie signed with
   * the server-known JWT_SECRET (local + HCL daily + previews).
   * `"storage-state"` replays a previously-captured Playwright storage
   * state from `bots-app login` (for `app.videocall.rs` and any other
   * real-OAuth-protected target). See `src/auth/storage-state.ts`.
   */
  authBackend: AuthBackend;
  /**
   * When `authBackend === "storage-state"`, the absolute path to the
   * captured `<account>.json` file. Ignored in JWT mode.
   */
  storageStateFile?: string | null;
  /**
   * When provided alongside `runDir`, the bot looks up the prep'd fake
   * camera (y4m) + fake mic (WAV) for this participant and passes them
   * to Chrome via `--use-file-for-fake-{video,audio}-capture`. When
   * either of the resolved files is missing, the bot falls back to
   * Chrome's default fake-device pattern for that media kind and logs
   * a warning. Pass `manifest = null` (or omit both) to skip the
   * lookup entirely (the launch then uses default fake devices).
   */
  manifest?: Manifest | null;
  runDir?: string | null;
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

  const launchArgs = [...CHROME_ARGS];
  if (opts.manifest != null && opts.runDir != null && opts.runDir !== "") {
    const assets = resolveAssetsForParticipant({
      manifest: opts.manifest,
      runDir: opts.runDir,
      participant: opts.participant,
    });
    if (assets.audioPath !== null) {
      launchArgs.push(`--use-file-for-fake-audio-capture=${assets.audioPath}`);
      console.log(`[${opts.participant}] fake mic → ${assets.audioPath}`);
    } else {
      console.warn(
        `[${opts.participant}] no stitched WAV found under ${opts.runDir}/audio — using Chrome's default fake mic. Run \`npm run bot -- prep-assets\` to fix.`,
      );
    }
    if (assets.videoPath !== null) {
      launchArgs.push(`--use-file-for-fake-video-capture=${assets.videoPath}`);
      console.log(`[${opts.participant}] fake camera → ${assets.videoPath}`);
    } else {
      console.warn(
        `[${opts.participant}] no costume y4m found under ${opts.runDir}/costumes — using Chrome's default fake camera. Run \`npm run bot -- prep-assets\` to fix (or the participant has no costume_dir).`,
      );
    }
  }

  const browser = await chromium.launch({
    headless: opts.headless,
    args: launchArgs,
  });
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    storageState:
      opts.authBackend === "storage-state" && opts.storageStateFile
        ? requireStorageState(opts.storageStateFile)
        : undefined,
  });
  if (opts.authBackend === "jwt") {
    const email = participantEmail(opts.participant);
    await applyJwtCookieAuth(context, {
      email,
      displayName: opts.displayName,
      baseURL,
    });
    console.log(`[${opts.participant}] auth: jwt (injected session cookie for ${email})`);
  } else {
    console.log(
      `[${opts.participant}] auth: storage-state (reused captured session from ${opts.storageStateFile})`,
    );
  }

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
