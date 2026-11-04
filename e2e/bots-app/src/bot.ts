import { existsSync } from "node:fs";
import { COSTUME_HEIGHT, COSTUME_WIDTH, y4mMatchesTargetGeometry } from "./costumes";
import { join } from "node:path";

import { chromium, Browser, BrowserContext, Page } from "@playwright/test";

import { applyJwtCookieAuth } from "./auth/jwt-cookie";
import { performFormLogin, resolveFormLoginCredentials } from "./auth/form-login";
import { type AuthBackend, requireStorageState } from "./auth/storage-state";
import { resolveAssetsForParticipant } from "./assets";
import { ensureAssetsPrimed, type PrimeProgress } from "./auto-prime";
import { browserEnvWithoutSecrets } from "./browser-env";
import {
  type CameraCycleConfig,
  type CameraCycleRunner,
  CAMERA_CYCLE_DEGRADED_BANNER,
  formatCameraCycleConfig,
  startCameraCycle,
} from "./camera-cycle";
import { HANG_UP_CANDIDATES, resolveControlSelector } from "./control-buttons";
import {
  buildDecodeBudgetInitScript,
  type DecodeBudget,
  DECODE_BUDGET_OFF_VALUE,
  DECODE_BUDGET_STORAGE_KEY,
  DecodeBudgetNotAppliedError,
  isClientDecodeBudget,
} from "./decode-budget";
import { isDevServerNoise } from "./dev-noise";
import {
  isLoadFailure,
  joinedDocumentIdentity,
  PAGE_RELOADED,
  type PageReload,
  PageReloadedError,
  STAMP_TIMEOUT_MS,
  type StampResult,
  StampTimeoutError,
  watchPageReload,
} from "./page-reload";
import { taggedLine } from "./log-line";
import { type Manifest } from "./manifest";
import { captureGeometryToken, type SourceGeometry } from "./posture";
import { effectiveVideoNoise, videoNoiseLine } from "./video-noise";
import {
  buildClockInitScript,
  parseTalkingClockLine,
  resolveClockVoice,
  SOLO_CLOCK_SLOT,
  talkingClockLine,
  type TalkingClockSlot,
} from "./clock-init";
import { SPEECH_PLAN_DEFAULT, type SpeechPlan } from "./speech-plan";
import { type BotPersona, createPersonaPlanner, mutedListeners, personaTargets } from "./persona";
import {
  type PersonaDriver,
  personaReceipt,
  type PersonaTally,
  startPersonaDriver,
} from "./persona-driver";
import {
  buildReceiverConfigInitScript,
  buildReceiverConfigOverrides,
  type DiagPackets,
} from "./receiver-caps";
import { coerceEncoderFps } from "./resource/fps";
import {
  SIMULCAST_MAX_LAYERS_CONFIG_KEY,
  SIMULCAST_READBACK_TIMEOUT_MS,
  SIMULCAST_READBACK_UNREAD,
  type SimulcastMaxLayersObserved,
  SimulcastMaxLayersNotAppliedError,
  SimulcastMaxLayersReadbackTimeoutError,
  simulcastMaxLayersReadback as readSimulcastMaxLayers,
} from "./simulcast-layers";
import {
  BROWSER_STEP_TIME_LIMIT_MS,
  PAGE_CALL_TIME_LIMIT_MS,
  TimeLimitExceededError,
  withTimeLimit,
} from "./time-limit";
import {
  buildTransportInitScript,
  isPinned,
  parseTransportLine,
  type TransportApplied,
  type TransportEvent,
  type TransportIntent,
  transportMismatch,
  TransportNotAppliedError,
} from "./transport";
import {
  GRID_SELECTOR,
  isSessionUserIdResponse,
  JOIN_MEDIA_ON,
  type JoinMedia,
  JoinMediaNotAppliedError,
  type JoinMediaState,
  joinMeetingAndEnableMedia,
  JoinRejectedError,
  MeetingNavigatedAwayError,
  sessionUserIdFromBody,
  WaitingRoomError,
} from "./meeting-join";
import {
  buildSpeakingGlowInitScript,
  type SpeakingGlow,
  SPEAKING_GLOW_OFF_VALUE,
  SPEAKING_GLOW_STORAGE_KEY,
  SpeakingGlowNotAppliedError,
  speakingGlowVerdict,
} from "./speaking-glow";

const CHROME_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-dev-shm-usage",
];

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

/** Bounds on each failed-launch teardown step, so a stalled renderer cannot hold the launch open. */
export const HANG_UP_BUDGET_MS = 5_000;
export const CLOSE_STEP_BUDGET_MS = 5_000;

/** The launch was aborted (shutdown or run deadline) before the bot was handed back. */
export class LaunchAbortedError extends Error {
  constructor() {
    super("launch aborted by shutdown or the run deadline; browser closed");
    this.name = "LaunchAbortedError";
  }
}

async function within(work: Promise<unknown>, ms: number): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  await Promise.race([
    work.catch(() => {}),
    new Promise<void>((resolve) => {
      timer = setTimeout(resolve, ms);
    }),
  ]);
  clearTimeout(timer);
}

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

export interface DiagPacketsObservation {
  state: "ENABLED" | "DISABLED";
  source: "url" | "config" | "default";
}

/** dioxus-ui `attendants.rs` logs this once per meeting mount (#2970). */
export const DIAG_PACKETS_LINE =
  /diagnostics packets: (ENABLED|DISABLED) \(source=(url|config|default)\)/;

export function diagPacketsExpected(requested: DiagPackets): DiagPacketsObservation["state"] {
  return requested === "on" ? "ENABLED" : "DISABLED";
}

export function parseDiagPacketsLine(text: string): DiagPacketsObservation | null {
  const m = DIAG_PACKETS_LINE.exec(text);
  if (m === null) return null;
  return { state: m[1], source: m[2] } as DiagPacketsObservation;
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
  /** Epoch ms the JWT-mode session cookie must expire by. */
  sessionNotAfterMs?: number | null;
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
  /** Injects `__APP_CONFIG.experimentalSimulcastMaxLayers` and fails the launch unless it reads back. */
  simulcastMaxLayers?: number | null;
  /** `"off"` stops this bot's own DIAGNOSTICS sends (#2970); a fidelity trade. */
  diagPackets?: DiagPackets | null;
  /** Receives each `diagnostics packets:` console line the client prints. */
  onDiagPackets?: ((obs: DiagPacketsObservation) => void) | null;
  /** `"off"` seeds `vc_decode_budget_override=all` and fails the launch unless it reads back; a fidelity waiver. */
  decodeBudget?: DecodeBudget | null;
  /** `"off"` seeds `vc_appearance_glow_enabled=false` and fails the launch unless it reads back; a fidelity waiver. */
  speakingGlow?: SpeakingGlow | null;
  /** Override `FORM_LOGIN_TIMEOUT_MS`; the login runs over startup shaping (#2354). */
  formLoginTimeoutMs?: number | null;
  /** Override `FORM_LOGIN_ACTION_TIMEOUT_MS` for the per-step fill/click actions. */
  formLoginActionTimeoutMs?: number | null;
  /** Clock-mode capture geometry (#2236). */
  sourceGeometry: SourceGeometry;
  /** Clock-mode background noise alpha (issue 3089); unset = the geometry's default. */
  videoNoise?: number | null;
  /** Clock-mode index in a talking-clock fleet (issue 3072); unset = index 0 of 1, i.e. speaks every turn. */
  talkingClock?: TalkingClockSlot | null;
  speechPlan?: SpeechPlan | null;
  /** Opt-in duty cycle (#2362). Set = less publish. */
  cameraCycle?: CameraCycleConfig | null;
  /** Clock mode, issue 3091: mic and camera follow `speechPlan`. */
  persona?: BotPersona | null;
  onPersona?: ((tally: PersonaTally) => void) | null;
  /** Opt-in camera-off / mic-muted join (#2914); unset = both turned on after joining. */
  joinMedia?: JoinMedia | null;
  /** Aborting closes the browser and rejects the launch with `LaunchAbortedError`. */
  signal?: AbortSignal | null;
  /** Pinned: injects `defaultTransport`, clears remembered choices, fails on a mismatching applied line. Unset = `auto`. */
  transport?: TransportIntent | null;
  /** Receives each `Transport preference applied:` / `Election decision:` line the client prints. */
  onTransport?: ((event: TransportEvent) => void) | null;
}

/** How long after join a pinned bot waits for its first `Transport preference applied:` line. */
export const TRANSPORT_APPLIED_WAIT_MS = 10_000;

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
 *   - `page-reloaded`   : the meeting page loaded a new document after join.
 */
export type BotExitReason =
  | { kind: "ttl-expired" }
  | { kind: "shutdown-signal" }
  | { kind: "user-hangup" }
  | { kind: "waiting-room"; variant: "waiting-room" | "waiting-for-host"; detail: string }
  | { kind: "meeting-rejected"; reason: "rejected" | "error"; detail: string }
  | { kind: "launch-error"; cause: unknown }
  | { kind: "browser-crash"; detail: string }
  | { kind: "page-reloaded"; detail: string };

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
  /** Close the browser at once, as a deliberate close rather than a crash, whatever `shutdown` is doing. */
  closeBrowserNow: () => Promise<void>;
  /**
   * Resolves when the user manually leaves the meeting via the browser
   * (top-frame URL transitions away from `/meeting/…`). The orchestrator
   * races this against the TTL + shutdown signal so a manual hang-up
   * shuts the bot down promptly rather than waiting out the TTL.
   *
   * Resolves at most once per bot; rejection is not possible.
   */
  userHangupDetected: Promise<void>;
  /** Resolves with a description when the browser or page dies without our asking. */
  crashDetected: Promise<string>;
  /** Resolves when the meeting page loads a new document after join, which leaves the meeting. */
  pageReloaded: Promise<PageReload>;
  /** Media state the join step left behind. */
  joinMedia: JoinMediaState;
  /** The `user_id` the meeting-api handed this session, once a participant-status response named it. */
  sessionUserId: () => string | null;
  /** `vc_decode_budget_override` read back after join (only for `off` or a replayed storage state); `null` otherwise. */
  decodeBudgetReadback: string | null;
  /** `vc_appearance_glow_enabled` read back after join (only for `off` or a replayed storage state); `null` otherwise. */
  speakingGlowReadback: string | null;
  /** `__APP_CONFIG.skipCanvasPaint` read back after join; `null` when not injected or unreadable. */
  skipCanvasPaintReadback: string | null;
  /** `__APP_CONFIG.experimentalSimulcastMaxLayers` read back after join; `null` when absent, `"unread"` when the read timed out or threw. */
  simulcastMaxLayersReadback: SimulcastMaxLayersObserved;
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

/** Aborting `opts.signal` before the launch resolves closes the browser and rejects with `LaunchAbortedError`; any other failure after Chromium starts closes it before rejecting. */
export async function launchBot(opts: BotRunOptions): Promise<BotHandle> {
  const signal = opts.signal ?? null;
  const isAborted = (): boolean => signal?.aborted === true;
  if (isAborted()) throw new LaunchAbortedError();
  const close: { fn: (() => Promise<void>) | null; abandon: (() => Promise<void>) | null } = {
    fn: null,
    abandon: null,
  };
  let rejectAborted!: (e: Error) => void;
  const aborted = new Promise<never>((_resolve, reject) => {
    rejectAborted = reject;
  });
  aborted.catch(() => {});
  const onAbort = (): void => {
    void close.fn?.();
    rejectAborted(new LaunchAbortedError());
  };
  signal?.addEventListener("abort", onAbort, { once: true });
  const body = launchInBrowser(opts, {
    registerClose: (fn, abandon) => {
      close.fn = fn;
      close.abandon = abandon;
      if (isAborted()) void fn();
    },
    aborted: isAborted,
  });
  body.catch(() => {});
  let handle: BotHandle;
  try {
    handle = await Promise.race([body, aborted]);
  } catch (e) {
    if (!isAborted()) await close.abandon?.();
    if (!isAborted()) throw e;
    await within(
      Promise.allSettled([body, close.fn?.() ?? Promise.resolve()]),
      2 * CLOSE_STEP_BUDGET_MS,
    );
    throw new LaunchAbortedError();
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
  if (isAborted()) {
    await within(handle.shutdown(), 2 * CLOSE_STEP_BUDGET_MS);
    throw new LaunchAbortedError();
  }
  return handle;
}

async function launchInBrowser(
  opts: BotRunOptions,
  launch: {
    registerClose: (close: () => Promise<void>, abandon: () => Promise<void>) => void;
    aborted: () => boolean;
  },
): Promise<BotHandle> {
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
    env: browserEnvWithoutSecrets(),
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
  // intentional close goes through `abandonBrowser()`, which raises the flag
  // first. Setting it only in `shutdown()` is not enough: the early exits would
  // each log a false crash on a graceful hang-up, which is exactly the spurious
  // error line #2089 exists to remove.
  let intentionalCloseStarted = false;
  let browserDiedUnexpectedly = false;
  let crashSeen: string | null = null;
  let resolveCrash!: (detail: string) => void;
  const crashDetected = new Promise<string>((resolve) => {
    resolveCrash = (detail) => {
      crashSeen ??= detail;
      resolve(detail);
    };
  });
  browser.on("disconnected", () => {
    if (!intentionalCloseStarted) {
      browserDiedUnexpectedly = true;
      resolveCrash("browser disconnected unexpectedly");
      console.error(at(`browser disconnected unexpectedly (crash or external kill)`));
    }
  });
  const closeStep = (what: "context" | "browser", close: () => Promise<void>): Promise<void> =>
    withTimeLimit(`${what}.close`, CLOSE_STEP_BUDGET_MS, close).catch((e: unknown) => {
      if (isBenignTeardownError(e, browserDiedUnexpectedly)) {
        console.debug(at(`${what} was already closed during teardown:`), e);
      } else {
        console.error(at(`${what}.close failed:`), e);
      }
    });
  let browserClosing: Promise<void> | null = null;
  // Every close disarms the reload watch first, so the close is never read as a reload.
  let disarmReloadWatch = (): void => {};
  const abandonBrowser = (): Promise<void> => {
    disarmReloadWatch();
    intentionalCloseStarted = true;
    browserClosing ??= closeStep("browser", () => browser.close());
    return browserClosing;
  };
  /**
   * The one teardown every path shares: each step bounded, run once. The flag is
   * raised before `browser.close()`, not `context.close()`, so a `disconnected`
   * during the context close still reads as a crash.
   */
  let createdContext: BrowserContext | null = null;
  let tearingDown: Promise<void> | null = null;
  const closeBrowserIntentionally = (): Promise<void> => {
    disarmReloadWatch();
    const context = createdContext;
    tearingDown ??= (async () => {
      if (context !== null) await closeStep("context", () => context.close());
      await abandonBrowser();
    })();
    return tearingDown;
  };
  launch.registerClose(closeBrowserIntentionally, abandonBrowser);
  const setupStep = <T>(what: string, step: () => Promise<T>): Promise<T> =>
    withTimeLimit(what, BROWSER_STEP_TIME_LIMIT_MS, step);
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
  const context = await setupStep("browser.newContext", () =>
    browser.newContext({
      ignoreHTTPSErrors: true,
      storageState: initialStorageState,
      ...BOT_RECEIVE_POSTURE,
    }),
  );
  createdContext = context;
  if (opts.authBackend === "jwt") {
    const email = participantEmail(opts.participant);
    await setupStep("session cookie injection", () =>
      applyJwtCookieAuth(context, {
        email,
        displayName: opts.displayName,
        baseURL,
        notAfterSecs:
          opts.sessionNotAfterMs == null ? undefined : Math.floor(opts.sessionNotAfterMs / 1000),
      }),
    );
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

  const page = await setupStep("context.newPage", () => context.newPage());
  const addInitScript = (source: string): Promise<void> =>
    setupStep("page.addInitScript", () => page.addInitScript(source));
  const hangUpWithinBudget = (): Promise<void> =>
    within(
      clickHangUp(
        page,
        (m, e) => console.error(at(m), e),
        (m) => console.warn(at(m)),
      ),
      HANG_UP_BUDGET_MS,
    );
  page.on("crash", () => {
    console.error(at("page crashed"));
    resolveCrash("page crashed");
  });

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
    await addInitScript(
      `Object.defineProperty(navigator, 'hardwareConcurrency', { get: () => ${cores}, configurable: true });`,
    );
    console.log(at(`navigator.hardwareConcurrency spoofed → ${cores} (issue #2035)`));
  }

  // Issues #2068/#2069 (increment 5): optionally cap the RECEIVED simulcast
  // layer and/or skip the per-tile canvas paint, to cut this bot's decode +
  // paint CPU. These are `window.__APP_CONFIG` overrides injected BEFORE the
  // first navigation via a setter-merge (the client parses __APP_CONFIG once
  // and prod config.js freezes it, so they are launch-time-only — see
  // receiver-caps.ts). When none is set, nothing is injected and the
  // deployment config is used verbatim (default behavior unchanged).
  const receiverOverrides = buildReceiverConfigOverrides({
    maxReceivedLayer: opts.maxReceivedLayer ?? undefined,
    skipCanvasPaint: opts.skipCanvasPaint ?? undefined,
    diagPackets: opts.diagPackets ?? undefined,
    defaultTransport: isPinned(opts.transport) ? opts.transport : undefined,
    simulcastMaxLayers: opts.simulcastMaxLayers ?? undefined,
  });
  if (isPinned(opts.transport)) {
    await addInitScript(buildTransportInitScript(originalUrl.origin));
  }
  if (receiverOverrides !== null) {
    await addInitScript(buildReceiverConfigInitScript(receiverOverrides, originalUrl.origin));
    console.log(
      at(`__APP_CONFIG overrides → ${JSON.stringify(receiverOverrides)} (#2068/#2069/#2970)`),
    );
  }

  if (opts.decodeBudget === "off") {
    await addInitScript(buildDecodeBudgetInitScript(originalUrl.origin));
    console.log(
      at(`decode budget off: seeding ${DECODE_BUDGET_STORAGE_KEY}=${DECODE_BUDGET_OFF_VALUE}`),
    );
  }

  if (opts.speakingGlow === "off") {
    await addInitScript(buildSpeakingGlowInitScript(originalUrl.origin));
    console.log(
      at(`speaking glow off: seeding ${SPEAKING_GLOW_STORAGE_KEY}=${SPEAKING_GLOW_OFF_VALUE}`),
    );
  }

  if (videoMode === "clock") {
    const slot = opts.talkingClock ?? SOLO_CLOCK_SLOT;
    const speechPlan = opts.speechPlan ?? SPEECH_PLAN_DEFAULT;
    const voice = resolveClockVoice(opts.participant, slot.index);
    const videoNoise = effectiveVideoNoise(opts);
    // ONE registration: clock-source.js reads these globals at module scope.
    await addInitScript(
      buildClockInitScript({
        displayName: opts.displayName,
        sourceGeometry: opts.sourceGeometry,
        videoNoise,
        slot,
        voice,
        speechPlan,
        mutedListeners: opts.persona ? mutedListeners(opts.persona.room) : [],
      }),
    );
    console.log(at(`fake camera: synchronized wall clock`));
    console.log(at(videoNoiseLine(opts)));
    console.log(at(talkingClockLine(slot, voice, speechPlan)));
    if (opts.persona) {
      const { mic, camera, index, room } = opts.persona;
      console.log(at(`persona: mic=${mic} camera=${camera} (index ${index} of ${room.fleetSize})`));
    }
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
    console.error(at(`pageerror: ${err.message}`));
  });
  page.on("console", (msg) => {
    const text = msg.text();
    if (msg.type() !== "error") return;
    if (isDevServerNoise(text, { pageUrl: page.url() })) {
      suppressedNoise++;
      return;
    }
    console.error(at(`console.error: ${text}`));
  });
  let personaDriver: PersonaDriver | null = null;
  page.on("console", (msg) => personaDriver?.onConsole(msg.text()));
  page.on("console", (msg) => {
    const status = parseTalkingClockLine(msg.text());
    if (status !== null) console.log(at(`talking clock: ${status}`));
  });
  page.on("console", (msg) => {
    const obs = parseDiagPacketsLine(msg.text());
    if (obs === null) return;
    console.log(at(`diagnostics packets: ${obs.state} (source=${obs.source})`));
    if (opts.diagPackets && obs.state !== diagPacketsExpected(opts.diagPackets)) {
      console.error(
        at(
          `--diag-packets ${opts.diagPackets} requested but the client logged ${obs.state} (source=${obs.source})`,
        ),
      );
    }
    opts.onDiagPackets?.(obs);
  });
  let lastApplied: TransportApplied | null = null;
  let notifyApplied: (() => void) | null = null;
  page.on("console", (msg) => {
    const event = parseTransportLine(msg.text());
    if (event === null) return;
    if (event.kind === "applied") {
      const a = event.applied;
      lastApplied = a;
      console.log(
        at(`transport applied: pref=${a.preference} source=${a.source} wt_urls=${a.wt_urls}`),
      );
      notifyApplied?.();
    }
    opts.onTransport?.(event);
  });

  let sessionUserId: string | null = null;
  const statusMeetingId = meetingIdFromUrl(opts.meetingURL);
  page.on("response", (res) => {
    if (!res.ok() || !isSessionUserIdResponse(res.url(), statusMeetingId)) return;
    void res
      .json()
      .then((body: unknown) => {
        sessionUserId = sessionUserIdFromBody(body) ?? sessionUserId;
      })
      .catch(() => {});
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
    if (isLoadFailure(frame.url())) return;
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

  const reloadWatch = watchPageReload(page, meetingPathPrefix, {
    identity: () => page.mainFrame().evaluate(() => performance.timeOrigin),
    joinedIdentity: () => page.mainFrame().evaluate(joinedDocumentIdentity, GRID_SELECTOR),
  });
  disarmReloadWatch = reloadWatch.disarm;
  const stampOrThrow = async (): Promise<StampResult> => {
    const result = await reloadWatch.stamp();
    if (result === "timed-out") throw new StampTimeoutError(STAMP_TIMEOUT_MS);
    const reload = reloadWatch.seen();
    if (reload !== null) throw new PageReloadedError(reload);
    return result;
  };
  const raceReload = <T>(work: Promise<T>): Promise<T> =>
    Promise.race([
      work,
      reloadWatch.detected.then((r): never => {
        throw new PageReloadedError(r);
      }),
    ]);
  const failIfReloaded = async (): Promise<void> => {
    const reload = reloadWatch.seen();
    if (reload === null) return;
    await closeBrowserIntentionally();
    throw new PageReloadedError(reload);
  };
  let joinMedia: JoinMediaState;
  let stamp: StampResult;
  try {
    // Pass the composite label (participant or participant@idshort)
    // through to the join helper so its log lines match the rest of
    // the bot's prefix.
    const joining = joinMeetingAndEnableMedia({
      page,
      participant: label,
      displayName: opts.displayName,
      meetingId,
      onInMeeting: async () => void (await stampOrThrow()),
      media: opts.joinMedia ?? JOIN_MEDIA_ON,
      signal: opts.signal ?? undefined,
    });
    joining.catch(() => {});
    // A reload ends the wait on the rename and media steps after the stamp, whatever they are awaiting.
    joinMedia = await raceReload(joining);
    stamp = await stampOrThrow();
  } catch (caught) {
    if (launch.aborted()) throw new LaunchAbortedError();
    const seenReload = reloadWatch.seen();
    const e =
      caught instanceof MeetingNavigatedAwayError && seenReload !== null
        ? new PageReloadedError(seenReload)
        : caught;
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
    if (e instanceof StampTimeoutError || e instanceof PageReloadedError) {
      const crashed = crashSeen as string | null;
      const failure = crashed === null ? e : new Error(`${crashed} during the launch`);
      console.error(at(failure.message));
      await closeBrowserIntentionally();
      throw failure;
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
    if (e instanceof JoinMediaNotAppliedError) {
      console.error(at(e.message));
      await hangUpWithinBudget();
      await closeBrowserIntentionally();
    }
    throw e;
  }

  if (stamp === "unreadable") {
    console.error(
      at(
        `WARNING: could not read the meeting document's identity after join: a page reload will not be detected for this bot`,
      ),
    );
  }
  const pageReloaded = reloadWatch.detected.then((r) => {
    console.error(
      at(
        `meeting page replaced its document at ${new Date(r.atMs).toISOString()} after join (${r.url}): the bot is no longer in the meeting; failing it as ${PAGE_RELOADED}`,
      ),
    );
    return r;
  });
  let simulcastMaxLayersReadback: SimulcastMaxLayersObserved = SIMULCAST_READBACK_UNREAD;
  let readbackStalled = false;
  try {
    const raw = await raceReload(
      withTimeLimit("simulcast read-back", SIMULCAST_READBACK_TIMEOUT_MS, () =>
        page.evaluate(
          (key) =>
            (globalThis as unknown as { __APP_CONFIG?: Record<string, unknown> }).__APP_CONFIG?.[
              key
            ],
          SIMULCAST_MAX_LAYERS_CONFIG_KEY,
        ),
      ),
    );
    simulcastMaxLayersReadback = readSimulcastMaxLayers(raw);
  } catch (e) {
    if (e instanceof TimeLimitExceededError) readbackStalled = true;
    else console.error(at(`simulcast max layers readback failed:`), e);
  }
  await failIfReloaded();
  const requestedLayers = opts.simulcastMaxLayers ?? null;
  if (readbackStalled && requestedLayers === null) {
    console.error(
      at(
        `WARNING: ${SIMULCAST_MAX_LAYERS_CONFIG_KEY} read-back did not settle within ${SIMULCAST_READBACK_TIMEOUT_MS}ms; recorded as unread`,
      ),
    );
  } else if (readbackStalled) {
    const err = new SimulcastMaxLayersReadbackTimeoutError(SIMULCAST_READBACK_TIMEOUT_MS);
    console.error(at(err.message));
    await abandonBrowser();
    throw err;
  } else if (requestedLayers !== null && simulcastMaxLayersReadback !== requestedLayers) {
    const err = new SimulcastMaxLayersNotAppliedError(requestedLayers, simulcastMaxLayersReadback);
    console.error(at(err.message));
    await clickHangUp(
      page,
      (m, e) => console.error(at(m), e),
      (m) => console.warn(at(m)),
    );
    await closeBrowserIntentionally();
    throw err;
  } else if (requestedLayers !== null) {
    console.log(
      at(
        `simulcast max layers verified: ${SIMULCAST_MAX_LAYERS_CONFIG_KEY}=${simulcastMaxLayersReadback}`,
      ),
    );
  }

  // Issues #2068/#2069: assert the receiver-cap overrides actually landed in
  // `window.__APP_CONFIG` once we are on `/meeting/<id>` (config.js has run and
  // our setter-merge fired). Best-effort + non-fatal: a mismatch means the
  // setter did not intercept the assignment (e.g. config.js switched to
  // `Object.defineProperty`, or the deployment predates the knobs) — surface it
  // loudly rather than let a silently-uncapped bot skew the load test. This
  // proves the override is PRESENT in __APP_CONFIG; whether the deployed client
  // HONORS it depends on it carrying videocall-client PR #2078.
  let skipCanvasPaintReadback: string | null = null;
  const receiverCaps = Object.fromEntries(
    Object.entries(receiverOverrides ?? {}).filter(([k]) => k !== SIMULCAST_MAX_LAYERS_CONFIG_KEY),
  );
  if (Object.keys(receiverCaps).length > 0) {
    try {
      const applied = await raceReload(
        withTimeLimit("receiver-caps read-back", PAGE_CALL_TIME_LIMIT_MS, () =>
          page.evaluate((keys) => {
            const cfg = (globalThis as unknown as { __APP_CONFIG?: Record<string, unknown> })
              .__APP_CONFIG;
            const out: Record<string, unknown> = {};
            for (const k of keys) out[k] = cfg?.[k];
            return out;
          }, Object.keys(receiverCaps)),
        ),
      );
      if (typeof applied.skipCanvasPaint === "string") {
        skipCanvasPaintReadback = applied.skipCanvasPaint;
      }
      const mismatched = Object.keys(receiverCaps).filter((k) => applied[k] !== receiverCaps[k]);
      if (mismatched.length === 0) {
        console.log(at(`receiver caps verified in __APP_CONFIG: ${JSON.stringify(applied)}`));
      } else {
        console.error(
          at(
            `WARNING: receiver caps did NOT land in __APP_CONFIG (expected ${JSON.stringify(
              receiverCaps,
            )}, got ${JSON.stringify(applied)}); the bot may not be capped (issues #2068/#2069)`,
          ),
        );
      }
    } catch (e) {
      console.error(at(`receiver-caps __APP_CONFIG assertion check failed:`), e);
    }
  }

  // A fresh context's storage is empty unless a storage state was replayed.
  let decodeBudgetReadback: string | null = null;
  if (opts.decodeBudget === "off" || initialStorageState !== undefined) {
    try {
      decodeBudgetReadback = await raceReload(
        withTimeLimit("decode-budget read-back", PAGE_CALL_TIME_LIMIT_MS, () =>
          page.evaluate((key) => localStorage.getItem(key), DECODE_BUDGET_STORAGE_KEY),
        ),
      );
    } catch (e) {
      console.error(at(`decode-budget readback failed:`), e);
    }
  }
  await failIfReloaded();
  if (opts.decodeBudget === "off") {
    if (decodeBudgetReadback !== DECODE_BUDGET_OFF_VALUE) {
      const err = new DecodeBudgetNotAppliedError(decodeBudgetReadback);
      console.error(at(err.message));
      await hangUpWithinBudget();
      await closeBrowserIntentionally();
      throw err;
    }
    console.log(
      at(`decode budget off verified: ${DECODE_BUDGET_STORAGE_KEY}=${decodeBudgetReadback}`),
    );
  } else if (!isClientDecodeBudget(decodeBudgetReadback)) {
    console.error(
      at(
        `WARNING: decode budget client requested but ${DECODE_BUDGET_STORAGE_KEY}=${decodeBudgetReadback} is in storage (a captured storage state?): this bot runs under that override`,
      ),
    );
  }

  let speakingGlowReadback: string | null = null;
  if (opts.speakingGlow === "off" || initialStorageState !== undefined) {
    try {
      speakingGlowReadback = await raceReload(
        withTimeLimit("speaking-glow read-back", PAGE_CALL_TIME_LIMIT_MS, () =>
          page.evaluate((key) => localStorage.getItem(key), SPEAKING_GLOW_STORAGE_KEY),
        ),
      );
    } catch (e) {
      console.error(at(`speaking-glow readback failed:`), e);
    }
  }
  await failIfReloaded();
  const glowVerdict = speakingGlowVerdict(opts.speakingGlow ?? "client", speakingGlowReadback);
  if (glowVerdict === "not-applied") {
    const err = new SpeakingGlowNotAppliedError(speakingGlowReadback);
    console.error(at(err.message));
    await hangUpWithinBudget();
    await closeBrowserIntentionally();
    throw err;
  } else if (glowVerdict === "inherited-off") {
    console.error(
      at(
        `WARNING: speaking glow client requested but ${SPEAKING_GLOW_STORAGE_KEY}=${speakingGlowReadback} is in storage (a captured storage state?): this bot runs with the glow off`,
      ),
    );
  } else if (opts.speakingGlow === "off") {
    console.log(
      at(`speaking glow off verified: ${SPEAKING_GLOW_STORAGE_KEY}=${speakingGlowReadback}`),
    );
  }

  if (isPinned(opts.transport)) {
    if (lastApplied === null) {
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, TRANSPORT_APPLIED_WAIT_MS);
        notifyApplied = () => {
          clearTimeout(timer);
          resolve();
        };
        void reloadWatch.detected.then(() => notifyApplied?.());
      });
      notifyApplied = null;
    }
    await failIfReloaded();
    const applied = lastApplied as TransportApplied | null;
    const reason = applied === null ? null : transportMismatch(opts.transport, applied);
    if (applied === null) {
      console.error(
        at(
          `WARNING: --transport ${opts.transport} requested but no 'Transport preference applied:' line within ${TRANSPORT_APPLIED_WAIT_MS}ms of joining: the applied transport is unverified`,
        ),
      );
    } else if (reason !== null) {
      const err = new TransportNotAppliedError(opts.transport, reason);
      console.error(at(err.message));
      await hangUpWithinBudget();
      await closeBrowserIntentionally();
      throw err;
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

  const persona = videoMode === "clock" ? (opts.persona ?? null) : null;
  if (persona !== null) {
    const planner = createPersonaPlanner(
      opts.speechPlan ?? SPEECH_PLAN_DEFAULT,
      persona.room.fleetSize,
    );
    personaDriver = startPersonaDriver({
      page,
      targets: (t) => personaTargets(planner, persona, t),
      joined: { mic: joinMedia.mic, camera: joinMedia.camera },
      log: (m) => console.log(at(m)),
      error: (m) => console.error(at(m)),
      onTally: (tally) => opts.onPersona?.(tally),
    });
  }

  let personaStopped = false;
  const stopPersona = (): void => {
    if (personaDriver === null || persona === null || personaStopped) return;
    personaStopped = true;
    const receipt = personaReceipt(
      `mic=${persona.mic} camera=${persona.camera}`,
      personaDriver.stop(),
    );
    if (receipt.degraded) console.error(at(receipt.line));
    else console.log(at(receipt.line));
  };

  // Before hang-up: a tick during it would find no control and fail.
  const leaveMeeting = async (): Promise<void> => {
    reloadWatch.disarm();
    stopPersona();
    await hangUpWithinBudget();
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
    stopPersona();
    if (cameraCycleRunner !== null) {
      const receipt = cameraCycleRunner.stop();
      cameraCycleRunner = null;
      if (receipt.banner === CAMERA_CYCLE_DEGRADED_BANNER) console.error(at(receipt.line));
      else console.log(at(receipt.line));
    }
    await closeBrowserIntentionally();
  };

  const closeBrowserNow = abandonBrowser;

  if (launch.aborted()) {
    await within(shutdown(), 2 * CLOSE_STEP_BUDGET_MS);
    throw new LaunchAbortedError();
  }
  const reloadedDuringLaunch = reloadWatch.seen();
  if (reloadedDuringLaunch !== null) {
    await shutdown();
    throw new PageReloadedError(reloadedDuringLaunch);
  }

  return {
    browser,
    context,
    page,
    leaveMeeting,
    shutdown,
    closeBrowserNow,
    userHangupDetected,
    crashDetected,
    pageReloaded,
    joinMedia,
    sessionUserId: () => sessionUserId,
    decodeBudgetReadback,
    skipCanvasPaintReadback,
    speakingGlowReadback,
    simulcastMaxLayersReadback,
  };
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
export function participantEmail(participant: string): string {
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
