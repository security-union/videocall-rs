import { existsSync } from "node:fs";

import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";
import { type AuthBackend, requireStorageState } from "./auth/storage-state";
import { resolveAssetsForParticipant } from "./assets";
import { type Manifest } from "./manifest";
import { joinMeetingAndEnableMedia } from "./meeting-join";

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
   * **Only consulted when `authBackend === "jwt"`.** Path to a captured
   * SSO storage-state file (typically `<runDir>/auth/hcl-sso.json` from
   * `bots-app sso-login`). When the file exists, its cookies are loaded
   * into the context *before* the JWT cookie is injected — letting the
   * bot pass through the HCL SSO portal without an interactive auth
   * step on every run. When the file is missing this is a no-op (the
   * bot still launches; the page-load will hit the SSO portal on the
   * first navigation if one is in the way).
   */
  ssoStateFile?: string | null;
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
  let initialStorageState: string | undefined;
  let ssoStateLoaded = false;
  if (opts.authBackend === "storage-state" && opts.storageStateFile) {
    initialStorageState = requireStorageState(opts.storageStateFile);
  } else if (
    opts.authBackend === "jwt" &&
    opts.ssoStateFile &&
    opts.ssoStateFile !== "" &&
    existsSync(opts.ssoStateFile)
  ) {
    initialStorageState = opts.ssoStateFile;
    ssoStateLoaded = true;
  }
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    storageState: initialStorageState,
  });
  if (opts.authBackend === "jwt") {
    const email = participantEmail(opts.participant);
    await applyJwtCookieAuth(context, {
      email,
      displayName: opts.displayName,
      baseURL,
    });
    if (ssoStateLoaded) {
      console.log(
        `[${opts.participant}] auth: jwt + SSO state from ${opts.ssoStateFile} (injected session cookie for ${email})`,
      );
    } else {
      console.log(`[${opts.participant}] auth: jwt (injected session cookie for ${email})`);
      if (opts.ssoStateFile && opts.ssoStateFile !== "" && !existsSync(opts.ssoStateFile)) {
        console.warn(
          `[${opts.participant}] no SSO state at ${opts.ssoStateFile} — if the target sits behind HCL SSO, the page-load will redirect to the SSO portal. Run \`bots-app sso-login\` once to capture it.`,
        );
      }
    }
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

  const meetingId = meetingIdFromUrl(opts.meetingURL);
  await joinMeetingAndEnableMedia({
    page,
    participant: opts.participant,
    displayName: opts.displayName,
    meetingId,
  });

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

/**
 * Extract the meeting id from a meeting URL of the form
 * `https://.../meeting/<id>`. Used by the join-flow helper when the bot
 * lands on the homepage form and needs to retype the id.
 */
function meetingIdFromUrl(meetingURL: string): string {
  const url = new URL(meetingURL);
  const parts = url.pathname.split("/").filter((p) => p.length > 0);
  const meetingIdx = parts.indexOf("meeting");
  if (meetingIdx >= 0 && meetingIdx + 1 < parts.length) {
    return parts[meetingIdx + 1];
  }
  // Fallback: last path segment.
  return parts[parts.length - 1] ?? "";
}
