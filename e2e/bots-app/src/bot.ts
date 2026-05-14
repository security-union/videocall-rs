import { existsSync } from "node:fs";

import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";
import { type AuthBackend, requireStorageState } from "./auth/storage-state";
import { resolveAssetsForParticipant } from "./assets";
import { isDevServerNoise } from "./dev-noise";
import { type Manifest } from "./manifest";
import { joinMeetingAndEnableMedia, MeetingNavigatedAwayError } from "./meeting-join";

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-dev-shm-usage",
];

export interface BotRunOptions {
  meetingURL: string;
  participant: string;
  /**
   * Optional short id (typically the first 8 hex chars of the bot's
   * Phase 4 UUID) embedded in the bot's log prefix so operators can
   * correlate stdout with `bots-app ctl list` rows. When unset, the
   * legacy `[participant]` prefix is used — preserving byte-for-byte
   * compatibility with the pre-Phase 4 single-bot run.
   */
  botIdShort?: string | null;
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
  /**
   * When set, appends `?netsim=<profile>` to the meeting URL before
   * navigating. Requires the videocall-client build to have
   * `--features netsim`. See discussion #793 phase 3.
   */
  network?: string | null;
}

/**
 * Reason the orchestrator should record for a finished bot task. Used by
 * `runSingleBotTask` to distinguish between "launch error" (real failure;
 * counts toward the orchestrator's "ended with an error" tally) and
 * "graceful early exit" (e.g. user clicked the in-browser hang-up
 * button; logged normally and does *not* count as a failure).
 *
 * Kept as a discriminated union (not a bare string) so callers can carry
 * a structured `cause` along with the reason — and so adding a new
 * variant is a compile-time signal at every callsite.
 */
export type BotExitReason =
  | { kind: "ttl-expired" }
  | { kind: "shutdown-signal" }
  | { kind: "user-hangup" }
  | { kind: "launch-error"; cause: unknown };

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
  /**
   * Resolves when the user manually leaves the meeting via the browser
   * (top-frame URL transitions away from `/meeting/…`). The orchestrator
   * races this against the TTL + shutdown signal so a manual hang-up
   * shuts the bot down promptly rather than waiting out the TTL.
   *
   * Resolves at most once per bot; rejection is not possible.
   */
  userHangupDetected: Promise<void>;
}

/**
 * Build the log-prefix label for the bot. Returns `participant` when
 * `botIdShort` is unset, `participant@<idshort>` when it is. Reuse
 * everywhere so the prefix is identical across launch / join /
 * shutdown.
 */
function logLabel(opts: Pick<BotRunOptions, "participant" | "botIdShort">): string {
  return opts.botIdShort ? `${opts.participant}@${opts.botIdShort}` : opts.participant;
}

export async function launchBot(opts: BotRunOptions): Promise<BotHandle> {
  const label = logLabel(opts);
  // `baseURL` is derived from the *original* URL (no query) so the
  // JWT session cookie's scope doesn't drift if a `?netsim=` param is
  // injected below. `target` is the URL we actually navigate to —
  // when `opts.network` is set, it carries the `?netsim=<profile>`
  // search param that the in-tab `videocall-client` parses at startup
  // (when built with `--features netsim`).
  const originalUrl = new URL(opts.meetingURL);
  const baseURL = `${originalUrl.protocol}//${originalUrl.host}`;
  const target = new URL(opts.meetingURL);
  if (opts.network && opts.network !== "") {
    target.searchParams.set("netsim", opts.network);
    console.log(`[${label}] netsim: applying profile '${opts.network}' via ?netsim=<profile>`);
  }

  const launchArgs = [...CHROME_ARGS];
  if (opts.manifest != null && opts.runDir != null && opts.runDir !== "") {
    const assets = resolveAssetsForParticipant({
      manifest: opts.manifest,
      runDir: opts.runDir,
      participant: opts.participant,
    });
    if (assets.audioPath !== null) {
      launchArgs.push(`--use-file-for-fake-audio-capture=${assets.audioPath}`);
      console.log(`[${label}] fake mic → ${assets.audioPath}`);
    } else {
      console.warn(
        `[${label}] no stitched WAV found under ${opts.runDir}/audio — using Chrome's default fake mic. Run \`npm run bot -- prep-assets\` to fix.`,
      );
    }
    if (assets.videoPath !== null) {
      launchArgs.push(`--use-file-for-fake-video-capture=${assets.videoPath}`);
      console.log(`[${label}] fake camera → ${assets.videoPath}`);
    } else {
      console.warn(
        `[${label}] no costume y4m found under ${opts.runDir}/costumes — using Chrome's default fake camera. Run \`npm run bot -- prep-assets\` to fix (or the participant has no costume_dir).`,
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
        `[${label}] auth: jwt + SSO state from ${opts.ssoStateFile} (injected session cookie for ${email})`,
      );
    } else {
      console.log(`[${label}] auth: jwt (injected session cookie for ${email})`);
      if (opts.ssoStateFile && opts.ssoStateFile !== "" && !existsSync(opts.ssoStateFile)) {
        console.warn(
          `[${label}] no SSO state at ${opts.ssoStateFile} — if the target sits behind HCL SSO, the page-load will redirect to the SSO portal. Run \`bots-app sso-login\` once to capture it.`,
        );
      }
    }
  } else if (opts.authBackend === "storage-state") {
    console.log(
      `[${label}] auth: storage-state (reused captured session from ${opts.storageStateFile})`,
    );
  } else {
    // `authBackend === "none"` — guest join. No cookie injection, no
    // storage-state replay. The browser context launches with a clean
    // cookie jar; the meeting page must allow guest landing for this
    // to work.
    console.log(`[${label}] auth: guest (no session cookie injected)`);
  }

  const page = await context.newPage();

  // Dioxus 0.7's `trunk serve` workflow injects noisy diagnostics on
  // every page load (HMR websocket failure + the SPA HTML being served
  // where the browser expected JS during build_id resolution). The
  // volume is high enough to drown actually-interesting errors, so we
  // suppress matching events and surface a single summary line on
  // shutdown. See `dev-noise.ts` for the matcher.
  let suppressedNoise = 0;
  page.on("pageerror", (err) => {
    if (isDevServerNoise(err.message, { pageUrl: page.url() })) {
      suppressedNoise++;
      return;
    }
    console.error(`[${label}] pageerror:`, err.message);
  });
  page.on("console", (msg) => {
    if (msg.type() !== "error") return;
    if (isDevServerNoise(msg.text(), { pageUrl: page.url() })) {
      suppressedNoise++;
      return;
    }
    console.error(`[${label}] console.error:`, msg.text());
  });

  const navigateUrl = target.toString();
  console.log(`[${label}] navigating to ${navigateUrl}`);
  await page.goto(navigateUrl, { waitUntil: "domcontentloaded" });

  // `meetingIdFromUrl` operates on the raw `opts.meetingURL` because
  // the meeting-id lives in the path, not the query — adding a
  // `?netsim=` search param does not affect it.
  const meetingId = meetingIdFromUrl(opts.meetingURL);

  // Detect manual hang-up at any point in the bot's lifetime. The same
  // signal is consumed by `joinMeetingAndEnableMedia` (to abort the
  // join cleanly via `MeetingNavigatedAwayError`) and by the
  // orchestrator (to shut down a running bot when the user dismisses
  // it from the browser).
  const meetingPathPrefix = `/meeting/${meetingId}`;
  let resolveUserHangup!: () => void;
  const userHangupDetected = new Promise<void>((resolve) => {
    resolveUserHangup = resolve;
  });
  let userHangupFired = false;
  page.on("framenavigated", (frame) => {
    if (frame.parentFrame() !== null) return; // top frame only
    let pathname: string;
    try {
      pathname = new URL(frame.url()).pathname;
    } catch {
      return;
    }
    if (!pathname.startsWith(meetingPathPrefix) && !userHangupFired) {
      userHangupFired = true;
      console.log(`[${label}] page navigated away from meeting (likely manual hang-up)`);
      resolveUserHangup();
    }
  });

  try {
    // Pass the composite label (participant or participant@idshort)
    // through to the join helper so its log lines match the rest of
    // the bot's prefix.
    await joinMeetingAndEnableMedia({
      page,
      participant: label,
      displayName: opts.displayName,
      meetingId,
    });
  } catch (e) {
    if (e instanceof MeetingNavigatedAwayError) {
      // Make sure the orchestrator-facing signal fires even if for some
      // reason the `framenavigated` handler ran after the join helper's
      // own detection (e.g. handler ordering during fast back-to-back
      // navigations).
      if (!userHangupFired) {
        userHangupFired = true;
        resolveUserHangup();
      }
      // Tear down quietly — caller (orchestrator) will see this via
      // `userHangupDetected` and skip the leaveMeeting step.
      await context.close().catch(() => {});
      await browser.close().catch(() => {});
      // Re-throw so the orchestrator's `launchBot` await sees the
      // typed sentinel and can branch on it.
      throw e;
    }
    throw e;
  }

  // Best-effort: log the suppression summary once, after a successful
  // join (so the user knows we filtered something rather than silently
  // dropping signal).
  if (suppressedNoise > 0) {
    console.log(
      `[${label}] suppressing ${suppressedNoise} Dioxus dev-server noise events; this is normal under \`trunk serve\``,
    );
  }

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
      console.error(`[${label}] leaveMeeting failed:`, e);
    }
  };

  const shutdown = async (): Promise<void> => {
    try {
      await context.close();
    } catch (e) {
      console.error(`[${label}] context.close failed:`, e);
    }
    try {
      await browser.close();
    } catch (e) {
      console.error(`[${label}] browser.close failed:`, e);
    }
  };

  return { browser, context, page, leaveMeeting, shutdown, userHangupDetected };
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
