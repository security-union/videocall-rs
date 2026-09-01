import { existsSync, readFileSync } from "node:fs";
import { COSTUME_HEIGHT, COSTUME_WIDTH, y4mMatchesTargetGeometry } from "./costumes";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";
import { performFormLogin, resolveFormLoginCredentials } from "./auth/form-login";
import { type AuthBackend, requireStorageState } from "./auth/storage-state";
import { resolveAssetsForParticipant } from "./assets";
import { ensureAssetsPrimed, type PrimeProgress } from "./auto-prime";
import {
  type CameraCycleConfig,
  type CameraCycleRunner,
  CAMERA_CYCLE_DEGRADED_BANNER,
  formatCameraCycleConfig,
  startCameraCycle,
} from "./camera-cycle";
import { HANG_UP_CANDIDATES, resolveControlSelector } from "./control-buttons";
import { isDevServerNoise } from "./dev-noise";
import { taggedLine } from "./log-line";
import { type Manifest } from "./manifest";
import { captureGeometryToken, type SourceGeometry } from "./posture";
import { buildReceiverConfigInitScript, buildReceiverConfigOverrides } from "./receiver-caps";
import { coerceEncoderFps } from "./resource/fps";
import {
  joinMeetingAndEnableMedia,
  JoinRejectedError,
  MeetingNavigatedAwayError,
  WaitingRoomError,
} from "./meeting-join";

const CLOCK_SOURCE_PATH = fileURLToPath(new URL("./clock-source.js", import.meta.url));
const CLOCK_SOURCE_SCRIPT = `${readFileSync(CLOCK_SOURCE_PATH, "utf8")}\n//# sourceURL=${CLOCK_SOURCE_PATH}`;

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-dev-shm-usage",
];

/**
 * Fleet-credential env-var names, matched for removal before the browser is
 * launched. In the K8s fleet (#2035) the StatefulSet injects the WHOLE
 * `bot-accounts` Secret into every pod via `envFrom` — so `process.env` holds
 * EVERY ordinal's `BOT_EMAIL_<N>`/`BOT_PASSWORD_<N>` (plus the single-mode
 * `BOT_EMAIL`/`BOT_PASSWORD`), plus the fleet-wide `BOT_CTL_TOKEN` (#2072) — the
 * control-API bearer token, a STRICTLY higher-value secret than any single bot
 * account since it drives `/netem` and `/leave` on every pod. Only the
 * orchestrator (this Node process) needs any of these; the Chromium subprocess —
 * which renders untrusted page JS and is the highest-risk component for a
 * sandbox escape — needs NONE of them. Stripping them from the browser's
 * environment is defense-in-depth against a renderer→browser escape reading the
 * fleet's credentials out of the process environment. (Page JavaScript cannot
 * read OS env, so this is not a live hole — but the browser process holding zero
 * fleet secrets is strictly better than it holding all of them.)
 */
const FLEET_CRED_ENV_RE = /^BOT_(EMAIL|PASSWORD|CTL_TOKEN)(_\d+)?$/;

/**
 * The bot's rendered receive posture — an explicit 1080p desktop (#2235).
 *
 * Geometry is an input to receive load, not a cosmetic: #1256 caps each peer's
 * decoded rung by tile height in DEVICE pixels, and `min_tile_width` bounds how
 * many peer tiles render at all — unset, Playwright's default decided both. Why
 * this size, and which rungs it moves (density-dependent): see the PR.
 *
 * `satisfies` is load-bearing: Playwright silently ignores a misspelled option
 * key, and the spread at the call site defeats excess-property checking.
 */
export const BOT_RECEIVE_POSTURE = {
  viewport: { width: 1920, height: 1080 },
  deviceScaleFactor: 1,
} as const satisfies NonNullable<Parameters<Browser["newContext"]>[0]>;

/**
 * `process.env` with every fleet-credential variable removed, as a string-only
 * record suitable for Playwright's `chromium.launch({ env })` (which REPLACES
 * the browser env rather than merging). Non-credential vars (PATH, DISPLAY, …)
 * are preserved so the browser launches normally.
 */
export function browserEnvWithoutFleetCreds(
  source: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(source)) {
    if (v !== undefined && !FLEET_CRED_ENV_RE.test(k)) out[k] = v;
  }
  return out;
}

/**
 * Delay (ms) between encoder-fps polls of `window.__videocall_encoder_fps`
 * (#2062). The client republishes the value on its ~5s health tick. The global
 * is a PERSISTED LEVEL, not an event, so any interval observes each published
 * value at least once; polling a bit faster than the publish cadence is just
 * beat-frequency safety (no phase alignment can skip a value), not a need to
 * "catch" transient events. Oversampling is harmless: the starvation verdict
 * keys off a TIME-based sustained sub-rung duration (#2064, see
 * resource/verdict.ts), and re-reading a held value cannot inflate a wall-clock
 * duration. This is the delay BETWEEN a poll settling and the next poll starting
 * (a self-throttling setTimeout chain — see below), not a fixed-rate interval,
 * so polls can never pile up in flight.
 */
export const ENCODER_FPS_POLL_MS = 2000;

/** Per-action budget for one camera toggle (hover, click, post-condition). */
export const CAMERA_TOGGLE_TIMEOUT_MS = 5000;

const ALREADY_CLOSED_MESSAGES = [
  "Target page, context or browser has been closed",
  "Page has been closed.",
  "Pipe has been closed",
] as const;

/**
 * Whether a `context.close()` / `browser.close()` rejection is the EXPECTED
 * "already torn down" race rather than a real failure.
 *
 * The message alone is NOT sufficient, and that distinction is the whole point of
 * the second argument. Playwright emits these same strings when Chromium CRASHES
 * or exits unexpectedly, so matching on the text alone would demote a genuine
 * crash's close rejection to `debug`. The `disconnected` listener in `launchBot`
 * does log the crash itself at error level, so that rejection is not the ONLY
 * signal — but it is the diagnostic that says what failed while tearing down (the
 * message and stack), and it belongs at error level next to the crash line rather
 * than buried in `debug`. Note the bot has no other error-level reporter on this
 * path: the encoder-fps poll swallows its rejections by design, since a transient
 * "execution context destroyed" during an in-meeting SPA navigation must not blind
 * the fps signal.
 *
 * `browserDiedUnexpectedly` is the disambiguator: if Playwright reported
 * `browser.on("disconnected")` while NO intentional close was in progress, the
 * browser went away on its own — a crash — and the close rejection must stay at
 * error level. Only when we initiated the teardown ourselves is an already-closed
 * target expected. (Playwright emits `disconnected` for a normal `browser.close()`
 * too, which is why the caller raises an "intentional close" flag before every
 * deliberate teardown rather than only inside `shutdown()`.)
 *
 * Anything that does not match a known message stays at error level regardless,
 * so a NEW close failure mode is never masked.
 */
export function isBenignTeardownError(e: unknown, browserDiedUnexpectedly: boolean): boolean {
  if (browserDiedUnexpectedly) return false;
  if (!(e instanceof Error)) return false;
  return ALREADY_CLOSED_MESSAGES.some((message) => e.message.includes(message));
}

export type VideoMode = "costume" | "file" | "clock";

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
   * Camera source mode. `"costume"` and `"file"` retain the existing
   * manifest/override-backed fake-device behavior. `"clock"` injects a
   * page-lifetime canvas source and skips all asset preparation and
   * Chrome fake-file capture flags.
   */
  videoMode?: VideoMode | null;
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
  /**
   * Directory the manifest's `audio_file` paths are anchored against.
   * Required for the auto-prime path to resolve per-line WAVs when
   * stitching a participant's audio; without it (or without a
   * `manifest`), the auto-prime is skipped and the bot falls through
   * to the existing "resolve already-prepped files" path.
   *
   * Set automatically by the CLI (`loadManifest` returns it alongside
   * the parsed manifest) and by the orchestrator (which loads the
   * manifest at startup for dashboard-launched bots).
   */
  manifestDir?: string | null;
  runDir?: string | null;
  /**
   * Optional override for the directory containing per-costume
   * `<name>/talking.mp4` files the auto-prime feeds into ffmpeg's
   * y4m conversion. Defaults to `<repoRoot>/bot/assets/costumes`.
   */
  costumeSource?: string | null;
  /**
   * Optional progress callback wired into `ensureAssetsPrimed`. The
   * CLI logs to `console.log`; the dashboard's orchestrator forwards
   * each event into the per-bot rolling log buffer so the `View logs`
   * dialog can render priming progress live.
   */
  onPrimeProgress?: ((p: PrimeProgress) => void) | null;
  /**
   * Optional basename (e.g. `pirate.y4m`) of an explicit costume file
   * the operator picked in the dashboard's launch form. When set, this
   * overrides the manifest auto-match: the bot uses
   * `<runDir>/costumes/<costumeOverride>` for `--use-file-for-fake-video-capture`
   * regardless of what the manifest says about this participant.
   *
   * Falls back to the default fake camera (with a warning log) when
   * the resolved path doesn't exist on disk. The orchestrator validates
   * the filename against directory traversal before forwarding it.
   */
  costumeOverride?: string | null;
  /**
   * Mirror of {@link costumeOverride} for the audio side. Expected to
   * be a basename like `alice.wav` under `<runDir>/audio/`.
   */
  audioOverride?: string | null;
  /**
   * When set, appends `?netsim=<profile>` to the meeting URL before
   * navigating. Requires the videocall-client build to have
   * `--features netsim`. See discussion #793 phase 3.
   */
  network?: string | null;
  /**
   * Spoof `navigator.hardwareConcurrency` to this value via a Playwright
   * `addInitScript` injected BEFORE the first navigation (issue #2035
   * increment 2).
   *
   * The served videocall-client caps encoded simulcast layers at
   * `min(experimentalSimulcastMaxLayers, capability_max_simulcast_layers())`,
   * and the capability sniff reads `navigator.hardwareConcurrency` at
   * Host-mount time (dioxus-ui `capability_check.rs`: `<6 cores → 1 layer`,
   * `6–9 → 2`, `>=10 → 3`). In a container, Chrome reports the *node's*
   * core count (e.g. 32) regardless of the pod's CPU limit, so every bot
   * would sniff a ceiling from the NODE's cores rather than a modelled client.
   * Setting this to a modelled client's ladder DEPTH — not to the pod's CPU
   * budget, which would read 4 and collapse the ladder to 1 rung — makes the
   * ceiling a deliberate choice. The fleet uses 10 (3 rungs); see cli.ts, #2248.
   *
   * This is a `navigator` property (not an `__APP_CONFIG` key), so the spoof
   * SURVIVES the app's runtime `window.__APP_CONFIG` reassignment. When
   * unset / `null` / `<= 0`, NOTHING is injected and the browser's real value
   * is used — default behavior unchanged.
   */
  hardwareConcurrency?: number | null;
  /**
   * Receives a capture/encode FPS reading from this bot. The orchestrator wires
   * this to a per-bot {@link FpsTracker}. bot.ts feeds it by polling
   * `window.__videocall_encoder_fps`, published by videocall-client #2057,
   * every {@link ENCODER_FPS_POLL_MS}. Positive readings are forwarded as
   * numbers. An absent/`undefined` value, or a `0` (which the client treats as
   * "encoder not started, not diagnostic"), is forwarded as `null` so the
   * tracker can reset a sustained-low run without recording a reading.
   */
  onEncoderFps?: ((fps: number | null) => void) | null;
  /**
   * Cap the RECEIVED simulcast layer via `window.__APP_CONFIG.maxReceivedLayer`
   * (issue #2068). `0` = base rung only (lowest decode CPU); `undefined`/`null`
   * = no receive cap (client ceiling fully open). Injected at launch through a
   * `window.__APP_CONFIG` setter-merge (see {@link buildReceiverConfigInitScript})
   * BEFORE the first navigation, because the client parses `__APP_CONFIG` once
   * and prod `config.js` freezes it — it is NOT runtime-toggleable. Only takes
   * effect against a deployment carrying videocall-client PR #2078; harmless
   * otherwise. Independent of {@link hardwareConcurrency} (which caps the bot's
   * ENCODE layers) — this caps what the bot DECODES.
   */
  maxReceivedLayer?: number | null;
  /**
   * Skip per-tile canvas paint via `window.__APP_CONFIG.skipCanvasPaint`
   * (issue #2069). `true` = decode-and-drop (saves paint/GPU only — decode
   * still runs; use {@link maxReceivedLayer} to cut decode CPU). `false` =
   * force paint on. `undefined`/`null` = inherit the deployment's value. Same
   * launch-time `__APP_CONFIG` injection + PR #2078 dependency as
   * {@link maxReceivedLayer}.
   */
  skipCanvasPaint?: boolean | null;
  /** Override `FORM_LOGIN_TIMEOUT_MS`; the login runs over startup shaping (#2354). */
  formLoginTimeoutMs?: number | null;
  /** Override `FORM_LOGIN_ACTION_TIMEOUT_MS` for the per-step fill/click actions. */
  formLoginActionTimeoutMs?: number | null;
  /** Clock-mode capture geometry (#2236). */
  sourceGeometry: SourceGeometry;
  /** Opt-in duty cycle (#2362); unset = camera on the whole run. Set = less publish. */
  cameraCycle?: CameraCycleConfig | null;
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
 *
 * Graceful (not counted toward the failure tally):
 *   - `ttl-expired`     : natural lifetime ran out.
 *   - `shutdown-signal` : SIGINT/SIGTERM or `ctl-leave`/`ctl-kill`.
 *   - `user-hangup`     : operator clicked the in-browser HangUp button.
 *   - `waiting-room`    : meeting page parked us in a Waiting Room or a
 *                         "host hasn't started yet" lobby; the bot did
 *                         join, it just has no admit rights here.
 *
 * Failure (counts toward the tally):
 *   - `meeting-rejected`: the host denied our join, OR the page reported
 *                         a server-side join error (meeting closed,
 *                         host gone, etc.).
 *   - `launch-error`    : everything else that prevented the join from
 *                         completing (timeout, browser crash, ...).
 */
export type BotExitReason =
  | { kind: "ttl-expired" }
  | { kind: "shutdown-signal" }
  | { kind: "user-hangup" }
  | { kind: "waiting-room"; variant: "waiting-room" | "waiting-for-host"; detail: string }
  | { kind: "meeting-rejected"; reason: "rejected" | "error"; detail: string }
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

export interface HangUpPage {
  locator(selector: string): {
    isVisible(options?: { timeout?: number }): Promise<boolean>;
    click(options?: { timeout?: number }): Promise<void>;
  };
  waitForURL(predicate: (url: URL) => boolean, options?: { timeout?: number }): Promise<void>;
}

/**
 * The post-click `waitForURL` gives the client-side `meeting_api::leave_meeting`
 * request time to reach the server before the caller tears the context down;
 * not reaching `/` is not an error.
 */
export async function clickHangUp(
  page: HangUpPage,
  error: (message: string, e: unknown) => void,
  warn: (message: string) => void = (m) => console.warn(m),
): Promise<void> {
  try {
    const selector = await resolveControlSelector(page, HANG_UP_CANDIDATES, "leaveMeeting", warn);
    if (selector === null) return;
    await page.locator(selector).click({ timeout: 2_000 });
    await page.waitForURL((url) => url.pathname === "/", { timeout: 2_000 }).catch(() => {});
  } catch (e) {
    error("leaveMeeting failed:", e);
  }
}

export async function launchBot(opts: BotRunOptions): Promise<BotHandle> {
  const label = logLabel(opts);
  const at = (msg: string): string => taggedLine(label, msg);
  const videoMode = opts.videoMode ?? "costume";
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
    console.log(at(`netsim: applying profile '${opts.network}' via ?netsim=<profile>`));
  }

  const launchArgs = [...CHROME_ARGS];
  // Two paths feed the fake-device flags here:
  //   1. manifest auto-match — resolve `<participant>` → costume/audio
  //      via the loaded conversation manifest (CLI + dashboard-default
  //      behavior; the dashboard's orchestrator caches the manifest at
  //      startup so dashboard-launched bots get it for free).
  //   2. explicit overrides — the dashboard's launch form lets the
  //      operator pick a specific costume/audio basename. When set,
  //      those win over the manifest-resolved files, but we still keep
  //      the manifest path as a fallback (so a typo in the override
  //      degrades to "auto-match" rather than "default fake pattern").
  //
  // Either source resolves to absolute paths fed to Chrome via
  // `--use-file-for-fake-{video,audio}-capture`. A missing file at the
  // resolved path falls back to Chrome's default pattern with a
  // warning — never a hard failure.
  //
  // Auto-prime: before resolving the prep'd files, check whether the
  // expected outputs are actually on disk + up-to-date. If they're
  // not, run the same `prepare*` helpers `bots-app prep-assets`
  // invokes — inline, so the operator doesn't have to remember the
  // batch step. SSH-hosted bots never reach this code path
  // (`spawnRemoteBot` bypasses `launchBot` entirely), so the
  // auto-prime is local-only by construction.
  if (
    videoMode !== "clock" &&
    opts.manifest != null &&
    opts.manifestDir != null &&
    opts.manifestDir !== "" &&
    opts.runDir != null &&
    opts.runDir !== ""
  ) {
    await ensureAssetsPrimed({
      manifest: opts.manifest,
      manifestDir: opts.manifestDir,
      runDir: opts.runDir,
      participant: opts.participant,
      costumeSource: opts.costumeSource ?? undefined,
      onProgress: (p) => {
        // CLI default: prefix every progress event with the bot's
        // label so the merged stdout stays readable when several
        // bots are priming in parallel. The dashboard orchestrator
        // overrides this via `opts.onPrimeProgress` to append into
        // the per-bot rolling log buffer instead (with the same
        // formatted line).
        const line = at(`auto-prime: ${p.step} — ${p.message}`);
        if (opts.onPrimeProgress) {
          opts.onPrimeProgress(p);
        } else {
          console.log(line);
        }
      },
    });
  }
  if (videoMode !== "clock" && opts.runDir != null && opts.runDir !== "") {
    const assets =
      opts.manifest != null
        ? resolveAssetsForParticipant({
            manifest: opts.manifest,
            runDir: opts.runDir,
            participant: opts.participant,
          })
        : { audioPath: null, videoPath: null };
    const audioPath = resolveOverrideOrAuto({
      override: opts.audioOverride,
      runDir: opts.runDir,
      subdir: "audio",
      autoPath: assets.audioPath,
      label,
      kind: "audio",
    });
    const videoPath = resolveOverrideOrAuto({
      override: opts.costumeOverride,
      runDir: opts.runDir,
      subdir: "costumes",
      autoPath: assets.videoPath,
      label,
      kind: "video",
    });
    if (audioPath !== null) {
      launchArgs.push(`--use-file-for-fake-audio-capture=${audioPath}`);
      console.log(at(`fake mic → ${audioPath}`));
    } else if (opts.manifest != null) {
      console.warn(
        at(
          `no stitched WAV found under ${opts.runDir}/audio — using Chrome's default fake mic. Run \`npm run bot -- prep-assets\` to fix.`,
        ),
      );
    }
    if (videoPath !== null) {
      launchArgs.push(`--use-file-for-fake-video-capture=${videoPath}`);
      console.log(at(`fake camera → ${videoPath}`));
    } else if (opts.manifest != null) {
      console.warn(
        at(
          `no costume y4m found under ${opts.runDir}/costumes — using Chrome's default fake camera. Run \`npm run bot -- prep-assets\` to fix (or the participant has no costume_dir).`,
        ),
      );
    }
  }

  const browser = await chromium.launch({
    headless: opts.headless,
    args: launchArgs,
    // Strip the fleet's BOT_EMAIL_*/BOT_PASSWORD_* from the browser subprocess's
    // environment (see browserEnvWithoutFleetCreds) — the renderer/browser is the
    // highest-risk process and never needs the credentials the orchestrator used.
    env: browserEnvWithoutFleetCreds(),
    // Do not let Playwright preempt the bots-app's process lifecycle policy
    // (#2089). Playwright defaults these to `true` (`browserType.js`
    // `_launchProcess`), installing signal handlers that call
    // `gracefullyCloseAll()` and immediately close every launched browser.
    //
    // That RACES our shutdown and is the root cause of this issue's symptom: the
    // orchestrator's signal handler only resolves a promise, after which the wait
    // loop `await`s `leaveMeeting()` (network I/O) BEFORE `shutdown()`. Playwright's
    // close lands during that await, so our `context.close()` finds an
    // already-closed target and logs at error level on every graceful pod
    // shutdown.
    //
    // On SIGTERM it also breaks the crash detector below: Playwright's close fires
    // `disconnected` with no intentional-close flag raised, which is
    // indistinguishable from a real crash. Opting out makes that premise true on
    // the orchestrator's graceful pod-shutdown path. Playwright's handler registry
    // is process-global, so the coexisting SSO recapture launch opts out too.
    //
    // Chrome is still not leaked: `addProcessHandlerIfNeeded("exit")` is installed
    // UNCONDITIONALLY by `processLauncher.js`, independent of these options, and
    // its `killSet` kills the browser process on Node exit.
    //
    // SIGINT is left at Playwright's default deliberately: its `sigintHandler`
    // calls `process.exit(130)` after closing, and Ctrl-C behaviour for a local
    // operator is out of scope for this fix. SIGHUP differs from SIGTERM: the
    // orchestrator does not handle it, so disabling Playwright's handler preserves
    // Node's default process termination instead of swallowing the signal after
    // only closing the browsers.
    handleSIGTERM: false,
    handleSIGHUP: false,
  });
  // Crash detector for the teardown-log classifier (#2089). Playwright emits the
  // same "…has been closed" messages for an expected double-close race AND for a
  // Chromium crash, so the message alone cannot distinguish them.
  //
  // Playwright ALSO emits `disconnected` during a perfectly normal
  // `browser.close()`, so the listener must know whether WE asked. Every
  // intentional teardown in this function — `shutdown()` and each early-exit
  // path (missing form-login creds, form-login failure, manual hang-up,
  // waiting-room / join-rejected) — therefore goes through
  // `closeBrowserIntentionally()`, which raises the flag FIRST. Setting it only
  // in `shutdown()` is not enough: the early exits would each log a false crash
  // on a graceful hang-up, which is exactly the spurious error line #2089 exists
  // to remove.
  let intentionalCloseStarted = false;
  let browserDiedUnexpectedly = false;
  browser.on("disconnected", () => {
    if (!intentionalCloseStarted) {
      browserDiedUnexpectedly = true;
      console.error(at(`browser disconnected unexpectedly (crash or external kill)`));
    }
  });
  /**
   * Tear down context + browser as a DELIBERATE act, swallowing errors. Use this
   * for every early-exit teardown; `shutdown()` does the same sequencing with its
   * own error-reporting closes.
   *
   * The flag is raised immediately before `browser.close()` and NOT before
   * `context.close()`. Closing a CONTEXT does not disconnect the browser, so a
   * `disconnected` arriving during the context close can only be a real crash or
   * an external kill — raising the flag earlier would blind the listener across
   * exactly the window where a crash is most likely, and demote the resulting
   * close error to `debug`.
   */
  const closeBrowserIntentionally = async (): Promise<void> => {
    await context.close().catch(() => {});
    intentionalCloseStarted = true;
    await browser.close().catch(() => {});
  };
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
    ...BOT_RECEIVE_POSTURE,
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
        at(
          `auth: jwt + SSO state from ${opts.ssoStateFile} (injected session cookie for ${email})`,
        ),
      );
    } else {
      console.log(at(`auth: jwt (injected session cookie for ${email})`));
      if (opts.ssoStateFile && opts.ssoStateFile !== "" && !existsSync(opts.ssoStateFile)) {
        console.warn(
          at(
            `no SSO state at ${opts.ssoStateFile} — if the target sits behind HCL SSO, the page-load will redirect to the SSO portal. Run \`bots-app sso-login\` once to capture it.`,
          ),
        );
      }
    }
  } else if (opts.authBackend === "storage-state") {
    console.log(at(`auth: storage-state (reused captured session from ${opts.storageStateFile})`));
  } else if (opts.authBackend === "form-login") {
    // The context launches with a clean cookie jar (like `"none"`); the
    // session is established later by driving the identity provider's
    // login form after the first navigation (see the form-login step
    // below, after `page.goto`).
    console.log(at(`auth: form-login (will drive the identity login form after navigation)`));
  } else {
    // `authBackend === "none"` — guest join. No cookie injection, no
    // storage-state replay. The browser context launches with a clean
    // cookie jar; the meeting page must allow guest landing for this
    // to work.
    console.log(at(`auth: guest (no session cookie injected)`));
  }

  const page = await context.newPage();

  // Issue #2035 (increment 2): optionally spoof `navigator.hardwareConcurrency`
  // BEFORE the first navigation. Playwright's addInitScript is evaluated after
  // the document is created but BEFORE any of the page's own scripts run — and
  // the WASM videocall-client (which reads `navigator.hardwareConcurrency` in
  // its simulcast capability sniff at Host-mount time) loads as a page script —
  // so the spoofed value is in place well before the client ever sniffs it, and
  // therefore controls the sniffed layer ceiling. In a container Chrome reports
  // the NODE's core count (e.g. 32) regardless of the pod's CPU limit, so every
  // bot would sniff a ceiling from the node's cores, not a modelled client;
  // pinning this to a modelled client's ladder depth (the fleet uses 10 -> 3
  // rungs) makes the ceiling deliberate. Defining an
  // OWN accessor on the `navigator` instance shadows the read-only prototype
  // getter that `web_sys::Navigator::hardware_concurrency()` reads; it is a
  // navigator property (not an `__APP_CONFIG` key) so it survives the app's
  // runtime config reassignment. Unset / `<= 0` ⇒ inject nothing (real value).
  if (opts.hardwareConcurrency != null && opts.hardwareConcurrency > 0) {
    const cores = Math.floor(opts.hardwareConcurrency);
    await page.addInitScript(
      `Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => ${cores}, configurable: true });`,
    );
    console.log(at(`navigator.hardwareConcurrency spoofed → ${cores} (issue #2035)`));
  }

  // Issues #2068/#2069 (increment 5): optionally cap the RECEIVED simulcast
  // layer and/or skip the per-tile canvas paint, to cut this bot's decode +
  // paint CPU. These are `window.__APP_CONFIG` overrides injected BEFORE the
  // first navigation via a setter-merge (the client parses __APP_CONFIG once
  // and prod config.js freezes it, so they are launch-time-only — see
  // receiver-caps.ts). When neither is set, nothing is injected and the
  // deployment config is used verbatim (default behavior unchanged).
  const receiverOverrides = buildReceiverConfigOverrides({
    maxReceivedLayer: opts.maxReceivedLayer ?? undefined,
    skipCanvasPaint: opts.skipCanvasPaint ?? undefined,
  });
  if (receiverOverrides !== null) {
    await page.addInitScript(buildReceiverConfigInitScript(receiverOverrides));
    console.log(
      at(`receiver caps → __APP_CONFIG ${JSON.stringify(receiverOverrides)} (issues #2068/#2069)`),
    );
  }

  if (videoMode === "clock") {
    // ONE registration: clock-source.js reads these globals at module scope.
    await page.addInitScript(
      `globalThis.__CLOCK_PARTICIPANT = ${JSON.stringify(opts.displayName)};\n` +
        `globalThis.__CLOCK_WIDTH = ${opts.sourceGeometry.width};\n` +
        `globalThis.__CLOCK_HEIGHT = ${opts.sourceGeometry.height};\n` +
        CLOCK_SOURCE_SCRIPT,
    );
    console.log(at(`fake camera: synchronized wall clock`));
  }

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
    console.error(at(`pageerror:`), err.message);
  });
  page.on("console", (msg) => {
    const text = msg.text();
    if (msg.type() !== "error") return;
    if (isDevServerNoise(text, { pageUrl: page.url() })) {
      suppressedNoise++;
      return;
    }
    console.error(at(`console.error:`), text);
  });

  const navigateUrl = target.toString();
  console.log(at(`navigating to ${navigateUrl}`));
  await page.goto(navigateUrl, { waitUntil: "domcontentloaded" });

  // `meetingIdFromUrl` operates on the raw `opts.meetingURL` because
  // the meeting-id lives in the path, not the query — adding a
  // `?netsim=` search param does not affect it. Computed here (before the
  // form-login step) because `performFormLogin` needs the `/meeting/<id>`
  // path to know when the post-callback navigation has settled.
  const meetingId = meetingIdFromUrl(opts.meetingURL);

  // Form-login runs HERE — after the first navigation but BEFORE the
  // `/meeting/<id>` hang-up detector (installed just below) and the join
  // flow. The identity provider's login page lives on a different origin
  // (e.g. `id.labsworkspace.fnxlabs.com`); if the hang-up detector were
  // already installed, that cross-origin navigation would be mistaken for
  // a manual hang-up and abort the launch. `performFormLogin` returns only
  // once the app has settled back on the `/meeting/<id>` path (past the
  // intermediate `/auth/callback` hop and any bounce through `/`), so the
  // detector installs cleanly afterward and the callback→meeting
  // navigation is not misread as a hang-up.
  if (opts.authBackend === "form-login") {
    const creds = resolveFormLoginCredentials(process.env);
    if (!creds) {
      // Tear the browser down before surfacing the error so a
      // mis-configured launch doesn't leak a Chrome process.
      await closeBrowserIntentionally();
      throw new Error(
        at(`auth: form-login requires BOT_EMAIL and BOT_PASSWORD environment variables to be set`),
      );
    }
    try {
      await performFormLogin({
        page,
        email: creds.email,
        password: creds.password,
        appBaseUrl: baseURL,
        meetingId,
        label,
        timeoutMs: opts.formLoginTimeoutMs ?? undefined,
        actionTimeoutMs: opts.formLoginActionTimeoutMs ?? undefined,
      });
    } catch (e) {
      await closeBrowserIntentionally();
      throw e;
    }
  }

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
      console.log(at(`page navigated away from meeting (likely manual hang-up)`));
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
      await closeBrowserIntentionally();
      // Re-throw so the orchestrator's `launchBot` await sees the
      // typed sentinel and can branch on it.
      throw e;
    }
    if (e instanceof WaitingRoomError || e instanceof JoinRejectedError) {
      // The meeting page reached a terminal-but-non-grid state. The
      // browser context is still alive but there's nothing for the bot
      // to do — tear it down and let the orchestrator classify the
      // exit (graceful for WaitingRoomError, failure for
      // JoinRejectedError).
      await closeBrowserIntentionally();
      throw e;
    }
    throw e;
  }

  // Issues #2068/#2069: assert the receiver-cap overrides actually landed in
  // `window.__APP_CONFIG` once we are on `/meeting/<id>` (config.js has run and
  // our setter-merge fired). Best-effort + non-fatal: a mismatch means the
  // setter did not intercept the assignment (e.g. config.js switched to
  // `Object.defineProperty`, or the deployment predates the knobs) — surface it
  // loudly rather than let a silently-uncapped bot skew the load test. This
  // proves the override is PRESENT in __APP_CONFIG; whether the deployed client
  // HONORS it depends on it carrying videocall-client PR #2078.
  if (receiverOverrides !== null) {
    try {
      const applied = await page.evaluate((keys) => {
        const cfg = (globalThis as unknown as { __APP_CONFIG?: Record<string, unknown> })
          .__APP_CONFIG;
        const out: Record<string, unknown> = {};
        for (const k of keys) out[k] = cfg?.[k];
        return out;
      }, Object.keys(receiverOverrides));
      const mismatched = Object.keys(receiverOverrides).filter(
        (k) => applied[k] !== receiverOverrides[k],
      );
      if (mismatched.length === 0) {
        console.log(at(`receiver caps verified in __APP_CONFIG: ${JSON.stringify(applied)}`));
      } else {
        console.error(
          at(
            `WARNING: receiver caps did NOT land in __APP_CONFIG (expected ${JSON.stringify(
              receiverOverrides,
            )}, got ${JSON.stringify(applied)}); the bot may not be capped (issues #2068/#2069)`,
          ),
        );
      }
    } catch (e) {
      console.error(at(`receiver-caps __APP_CONFIG assertion check failed:`), e);
    }
  }

  // Best-effort: log the suppression summary once, after a successful
  // join (so the user knows we filtered something rather than silently
  // dropping signal).
  if (suppressedNoise > 0) {
    console.log(
      at(
        `suppressing ${suppressedNoise} Dioxus dev-server noise events; this is normal under \`trunk serve\``,
      ),
    );
  }

  // #2062/#2057: poll the client's window global for encoder output fps and
  // feed the per-bot FpsTracker (contract: resource/fps.ts `coerceEncoderFps`).
  //
  // Scheduling: a SELF-THROTTLING setTimeout chain (next poll scheduled only
  // AFTER the previous `page.evaluate` settles), NOT a fixed-rate setInterval.
  // `page.evaluate` is a CDP round-trip that runs on the renderer's main JS
  // thread; under CPU saturation — the exact condition this harness measures —
  // a fixed-rate interval would fire again before the previous evaluate
  // resolved, piling up in-flight work and adding load precisely when the box
  // is most stressed. The chain applies at most one evaluate at a time.
  let fpsPollStopped = false;
  let fpsPollTimer: ReturnType<typeof setTimeout> | undefined;
  let pendingCaptureToken =
    videoMode === "clock" ? captureGeometryToken(label, opts.sourceGeometry) : null;
  if (opts.onEncoderFps) {
    const onFps = opts.onEncoderFps;
    const scheduleNextFpsPoll = (): void => {
      if (fpsPollStopped) return;
      fpsPollTimer = setTimeout(runFpsPoll, ENCODER_FPS_POLL_MS);
      // Do not let the poll timer keep the Node process alive on its own.
      fpsPollTimer.unref?.();
    };
    const runFpsPoll = (): void => {
      void page
        .evaluate(
          () =>
            (window as unknown as { __videocall_encoder_fps?: unknown }).__videocall_encoder_fps,
        )
        .then((raw) => {
          const fps = coerceEncoderFps(raw);
          onFps(fps);
          if (fps !== null && pendingCaptureToken !== null) {
            console.log(pendingCaptureToken);
            pendingCaptureToken = null;
          }
          scheduleNextFpsPoll();
        })
        .catch(() => {
          // A rejection may be terminal (page closed on teardown/hang-up) or
          // transient (e.g. "Execution context was destroyed" during an
          // in-meeting SPA navigation while the page stays alive). We keep
          // polling either way so a transient error never permanently blinds
          // the bot on the verdict-critical fps signal; `shutdown()` is the
          // authoritative stop (sets `fpsPollStopped`), and on a truly dead
          // page the next evaluate simply rejects again at the same cadence
          // (self-throttled, unref'd — no storm, no process hold).
          scheduleNextFpsPoll();
        });
    };
    scheduleNextFpsPoll();
  }

  // `shutdown()` is the authoritative stop and emits the receipt (#2362).
  let cameraCycleRunner: CameraCycleRunner | null = null;
  if (opts.cameraCycle != null) {
    console.log(
      at(
        `camera cycle configured — ${formatCameraCycleConfig(opts.cameraCycle)}; ` +
          `cameras-off time is published load this run will NOT represent (#2362)`,
      ),
    );
    cameraCycleRunner = startCameraCycle({
      page,
      config: opts.cameraCycle,
      timeoutMs: CAMERA_TOGGLE_TIMEOUT_MS,
      log: (m) => console.log(at(m)),
      error: (m) => console.error(at(m)),
    });
  }

  const leaveMeeting = async (): Promise<void> => {
    await clickHangUp(page, (m, e) => console.error(at(m), e));
  };

  const shutdown = async (): Promise<void> => {
    // Authoritative stop for the fps poll chain: set the flag so any in-flight
    // `page.evaluate` that settles after teardown does not reschedule, and
    // cancel a pending timer.
    fpsPollStopped = true;
    if (fpsPollTimer !== undefined) {
      clearTimeout(fpsPollTimer);
      fpsPollTimer = undefined;
    }
    if (cameraCycleRunner !== null) {
      const receipt = cameraCycleRunner.stop();
      if (receipt.banner === CAMERA_CYCLE_DEGRADED_BANNER) console.error(at(receipt.line));
      else console.log(at(receipt.line));
    }
    try {
      await context.close();
    } catch (e) {
      if (isBenignTeardownError(e, browserDiedUnexpectedly)) {
        console.debug(at(`context was already closed during teardown:`), e);
      } else {
        console.error(at(`context.close failed:`), e);
      }
    }
    // Raised HERE, not before `context.close()` above: closing a context does not
    // disconnect the browser, so a `disconnected` during the context close is a
    // genuine crash and must stay visible. Playwright DOES emit `disconnected` for
    // this `browser.close()`, which is what the flag suppresses.
    intentionalCloseStarted = true;
    try {
      await browser.close();
    } catch (e) {
      if (isBenignTeardownError(e, browserDiedUnexpectedly)) {
        console.debug(at(`browser was already closed during teardown:`), e);
      } else {
        console.error(at(`browser.close failed:`), e);
      }
    }
  };

  return { browser, context, page, leaveMeeting, shutdown, userHangupDetected };
}

/**
 * Resolve the final fake-device file path given an optional explicit
 * override basename and the manifest-resolved auto-match path. The
 * override is composed against `<runDir>/<subdir>/<override>` and used
 * verbatim when the file exists; if the file is missing we fall back
 * to the auto-match path (and log a warning so the operator notices
 * the typo). When no override is supplied, returns the auto-match path
 * directly. `null` means "no usable file found — Chrome will use its
 * default fake pattern".
 */
function resolveOverrideOrAuto(args: {
  override: string | null | undefined;
  runDir: string;
  subdir: string;
  autoPath: string | null;
  label: string;
  kind: "audio" | "video";
}): string | null {
  if (args.override && args.override !== "" && args.override !== "default") {
    const overridePath = join(args.runDir, args.subdir, args.override);
    if (existsSync(overridePath)) {
      // A VIDEO override bypasses auto-prime entirely (that only primes the
      // manifest-matched costume), so it is a third path a stale 1280x720 y4m can
      // reach Chrome on (#2171). Geometry-check it and fall back rather than
      // silently publishing 3x a real user's pixel load.
      if (args.kind === "video" && !y4mMatchesTargetGeometry(overridePath)) {
        console.warn(
          taggedLine(
            args.label,
            `video override "${args.override}" was built at a different geometry ` +
              `(expected ${COSTUME_WIDTH}x${COSTUME_HEIGHT}) — ignoring it and falling back to ` +
              `manifest auto-match. Re-run prep-assets to rebuild it.`,
          ),
        );
      } else {
        return overridePath;
      }
    } else {
      console.warn(
        taggedLine(
          args.label,
          `${args.kind} override "${args.override}" missing at ${overridePath} — falling back to manifest auto-match.`,
        ),
      );
    }
  }
  return args.autoPath;
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
