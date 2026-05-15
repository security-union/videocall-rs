import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { existsSync, readdirSync, statSync, unlinkSync } from "node:fs";
import { join } from "node:path";

import {
  defaultSsoStatePath,
  storageStatePath,
  DEFAULT_SSO_STATE_BASENAME,
} from "../auth/storage-state";
import {
  DEFAULT_SSO_START_URL,
  openSsoCaptureBrowser,
  type SsoCaptureSession,
} from "../auth/sso-capture";
import {
  costumeNameForParticipant,
  firstNParticipantNames,
  loadManifest,
  type Manifest,
} from "../manifest";
import {
  NETSIM_PRESETS,
  parseMeetingConfigText,
  seededRng,
  shuffleSeeded,
  type MeetingConfig,
} from "../meeting-config";
import { formatDuration, parseDuration, type Ttl } from "../ttl";
import { extractBearerToken, tokensMatch } from "./auth";
import {
  createPrepAssetsJob,
  type PrepAssetsJob,
  type PrepAssetsOptions,
  runPrepAssetsJob,
  sweepStalePrepAssetsJobs,
  validatePrepAssetsPath,
} from "./prep-assets";
import {
  deleteProfile,
  listProfiles,
  ProfileExistsError,
  ProfileNotFoundError,
  ProfileValidationError,
  readProfile,
  renameProfile,
  saveProfile,
  type ProfileBotSpec,
} from "./profiles";
import {
  type BotRegistryEntry,
  type BotSnapshot,
  NotSupportedRemoteError,
  readLocalLogWindow,
  snapshotEntry,
  sweepStaleEntries,
} from "./registry";
import {
  addHost,
  buildHostForPreview,
  getHost,
  listHosts,
  removeHost,
  SshHostExistsError,
  SshHostNotFoundError,
  SshHostValidationError,
  testHost,
  updateHost,
  type SshHostInput,
  type SshHostPatch,
} from "./ssh-hosts";
import { buildSshCommand, readLogWindow } from "./ssh-launcher";

/**
 * Callbacks the orchestrator exposes to the control server. Keeps the
 * HTTP layer ignorant of how bots are actually mutated — the server
 * only routes requests + validates payloads. The orchestrator owns
 * all state transitions, the in-flight `Promise<BotResult>` map, and
 * the `Map<string, BotRegistryEntry>` registry itself.
 *
 * Each handler returns a `ServiceResult` shaped to the JSON the
 * client sees. Errors thrown out of a handler surface as `500
 * internal error` with the message body — handlers can throw a
 * `ControlServerError` to render a specific status code (e.g. `404`
 * for an unknown bot id).
 */
export interface OrchestratorControlSurface {
  getRegistry(): Map<string, BotRegistryEntry>;
  /** Trigger graceful `leaveMeeting` + `shutdown` on a bot. */
  triggerLeave(botId: string): Promise<void>;
  /** Force-kill (skip leaveMeeting). Used by DELETE /bots/:id. */
  forceKill(botId: string): Promise<void>;
  /** Apply a new TTL (absolute "set" or "extend by"). */
  applyTtl(botId: string, newTtl: Ttl): void;
  /** Reconnect a bot with a new netsim profile. */
  changeNetwork(botId: string, network: string): Promise<void>;
  /** Click in-meeting mute/unmute. `mic === true` means muted. */
  setMicMuted(botId: string, micMuted: boolean): Promise<void>;
  /** Click in-meeting camera on/off. `cameraOff === true` means off. */
  setCameraOff(botId: string, cameraOff: boolean): Promise<void>;
  /** Click in-meeting screen-share toggle. `share === true` means share active. */
  setScreenShare(botId: string, share: boolean): Promise<void>;
  /** Spawn a duplicate; returns the new bot's id. */
  duplicateBot(
    sourceBotId: string,
    overrides: { participant?: string; ttl?: Ttl; network?: string },
  ): Promise<string>;
  /**
   * Spawn a fresh bot with the supplied fields. Returns the new bot's
   * id. Used by `POST /launch` from the phase-5 dashboard.
   *
   * `participantOverride` is the same handle the legacy CLI accepts —
   * implementations are responsible for resolving fake-device assets
   * and JWT subjects exactly as the `bots-app run` command does.
   */
  launchOne(spec: LaunchSpec): Promise<string>;
}

/**
 * Spec accepted by `OrchestratorControlSurface.launchOne`. Mirrors the
 * fields the dashboard's launch form sends. Validated by
 * `server.handleLaunch` before reaching the orchestrator.
 */
export interface LaunchSpec {
  meetingURL: string;
  participant: string;
  displayName?: string;
  ttl: Ttl;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
  /**
   * Absolute path to a captured SSO state file (typically
   * `<runDir>/auth/hcl-sso.json`). Only consulted when
   * `authBackend === "jwt"`. The dashboard sets this to the
   * conventional `defaultSsoStatePath(runDir)` when the file is
   * present so dashboard-launched bots transparently pick up the
   * captured HCL SSO session, matching the legacy CLI behavior.
   */
  ssoStateFile?: string;
  /**
   * Optional basename (e.g. `pirate.y4m`) of a costume the operator
   * picked in the dashboard's launch form. When set, overrides the
   * manifest-resolved costume for this participant. Validated against
   * the {@link ASSET_FILENAME_PATTERN} regex up-front so a path-like
   * value (`../etc/passwd`, `/abs/path.y4m`, etc.) cannot escape the
   * `<runDir>/costumes/` directory.
   */
  costume?: string;
  /**
   * Mirror of {@link costume} for the audio side. Expected to be a
   * basename like `alice.wav` under `<runDir>/audio/`.
   */
  audio?: string;
  /**
   * Where the bot's Chrome runs. Default is `{ kind: "local" }`
   * (back-compat with every pre-SSH caller). `{ kind: "ssh",
   * hostLabel }` looks up the host in the registry and runs the bot
   * over SSH on that host. Cloud-VM / Docker variants are tracked
   * separately and rejected by the validator today.
   */
  runLocation?: { kind: "local" } | { kind: "ssh"; hostLabel: string };
}

/**
 * Filename pattern accepted by the launch endpoint's `costume` /
 * `audio` fields. Matches the `/api/assets/*` listing convention
 * (basenames only, no path separators, no `..`). Rejecting anything
 * else server-side prevents a directory-traversal attack on the
 * fake-device path the orchestrator hands to Chrome.
 */
export const ASSET_FILENAME_PATTERN = /^[A-Za-z0-9_-]+\.(y4m|wav)$/;

/**
 * Default URL the VPN-status probe targets when `VPN_CHECK_URL` is not
 * set. We probe the production HCL host because that is the gated
 * surface bots need to reach — if it is unreachable from the operator's
 * host, no bot can join, and the dashboard should surface that
 * up-front rather than waiting for a per-bot launch timeout.
 *
 * `app.videocall.fnxlabs.com` redirects to HCL SSO when no session
 * cookie is present, so a real 401/302 here counts as "VPN up, just no
 * session"; we deliberately do NOT conflate the 401 case with "down".
 */
export const DEFAULT_VPN_CHECK_URL = "https://app.videocall.fnxlabs.com/";

/**
 * How long the SSO recapture endpoint's headed-Chrome session is
 * allowed to sit idle before it is force-cancelled and torn down. 15
 * minutes is generous for an operator who alt-tabs away mid-login but
 * tight enough that an abandoned session does not leak a Chrome
 * process for an entire workday.
 */
export const SSO_RECAPTURE_IDLE_TIMEOUT_MS = 15 * 60 * 1000;

/**
 * How long the `/assets/manifest` endpoint reuses a previously-built
 * response before re-parsing the YAML + re-stat'ing the per-participant
 * y4m / wav files. The manifest changes ~never during a dashboard
 * session (operators rerun `bots-app prep-assets` out-of-band), so a
 * 30s window cheaply absorbs the polling pattern the launch form uses
 * (60s refetchInterval) and any incidental refresh storms from
 * remounts.
 */
export const ASSETS_MANIFEST_CACHE_MS = 30_000;

/**
 * Server-side cap on the number of bots a single multi-launch /
 * from-config request may spawn. Matches the legacy CLI's
 * `--max-users 10` default. Operators who genuinely need a larger fleet
 * pass an explicit `maxUsers` field in the request body, but the
 * dashboard's form does not surface that knob to keep the UX simple.
 */
export const MULTI_LAUNCH_DEFAULT_MAX = 10;

/**
 * Filename pattern accepted by the OAuth-session endpoints. Allows
 * alphanumerics, hyphen, underscore, dot, and @-sign — matching the
 * legacy CLI's `bots-app login <account>` handle expectations (e.g.
 * `alice`, `alice.smith`, `alice@example.com`). Rejects any path
 * separator, leading dot, `..`, etc. so the resulting file cannot
 * escape `<runDir>/auth/`.
 */
export const OAUTH_LABEL_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._@+-]{0,127}$/;

/**
 * Bind options for the control server. `port: 0` (the default) asks
 * the kernel for a free ephemeral port — used by `--ctl-port auto`
 * on the CLI and by the in-process integration tests.
 */
export interface ControlServerOptions {
  port: number;
  token: string;
  surface: OrchestratorControlSurface;
  /**
   * Directory that holds persisted run-profile JSON files (one
   * per profile, under `<runDir>/profiles/`). When unset, the
   * `/profiles*` endpoints reply 503. Phase 5.1 feature.
   *
   * The SSO endpoints also use this to derive the conventional
   * `<runDir>/auth/hcl-sso.json` path. When unset, `/sso/status`
   * and `/sso/recapture` reply 503 — same pattern as `/profiles*`.
   */
  runDir?: string;
  /**
   * Absolute path to `bot/conversation/manifest.yaml` (or whichever
   * manifest the CLI was invoked with). The `/assets/manifest`
   * endpoint reads this to expose the participant → costume / audio
   * mapping the dashboard's launch form uses to auto-default the
   * costume + audio fields when an operator picks a participant name.
   *
   * When unset, the `/assets/manifest` endpoint replies with an empty
   * `participants` array — the dashboard treats that as "no manifest
   * available" and skips the auto-match logic entirely. Same fail-soft
   * shape the CLI uses when the manifest file is missing.
   */
  manifestPath?: string;
  /**
   * Injection seam for unit tests: when set, the VPN-status endpoint
   * calls this instead of the global `fetch`. Production callers
   * leave it unset and the endpoint uses the platform `fetch`.
   */
  vpnFetch?: typeof fetch;
  /**
   * Injection seam for unit tests: when set, the SSO recapture
   * endpoint calls this instead of {@link openSsoCaptureBrowser}. Lets
   * the suite exercise the spawn/save/cancel lifecycle without
   * actually launching Playwright Chromium.
   */
  ssoCaptureFactory?: (opts: { startUrl: string }) => Promise<SsoCaptureSession>;
  /**
   * Idle timeout (ms) applied to every recapture session. Defaults to
   * {@link SSO_RECAPTURE_IDLE_TIMEOUT_MS}. Overridable for tests using
   * fake timers.
   */
  ssoRecaptureIdleTimeoutMs?: number;
}

export interface ControlServerHandle {
  /** Actual bound port (resolved from `0` when `auto` was passed). */
  port: number;
  /** Underlying Node HTTP server. Exposed only for tests. */
  server: Server;
  /** Gracefully stop accepting new connections and close. */
  close(): Promise<void>;
  /**
   * Cancel and tear down every in-flight SSO recapture session. Called
   * by the orchestrator on SIGINT/SIGTERM so a stranded headed Chrome
   * process does not survive the parent's shutdown.
   */
  closeSsoRecaptureSessions(): Promise<void>;
}

/**
 * Per-session entry tracked in the SSO recapture map. The Node HTTP
 * server keeps these alive between `POST /sso/recapture` and the
 * subsequent `POST /sso/recapture/:id/complete` (or `DELETE`) call.
 */
interface SsoRecaptureEntry {
  id: string;
  startUrl: string;
  startedAt: number;
  session: SsoCaptureSession;
  idleTimer: ReturnType<typeof setTimeout>;
}

/**
 * Per-session entry tracked in the OAuth capture map. Mirrors
 * {@link SsoRecaptureEntry} but additionally records the operator-
 * supplied `label` — used to derive the save path
 * (`<runDir>/auth/<label>.json`) when the operator clicks "save".
 */
interface OauthCaptureEntry {
  id: string;
  label: string;
  startUrl: string;
  startedAt: number;
  session: SsoCaptureSession;
  idleTimer: ReturnType<typeof setTimeout>;
}

/**
 * One participant entry in the `/assets/manifest` response. `costumeFile`
 * and `audioFile` are basenames (e.g. `pirate.y4m`, `alice.wav`) the
 * dashboard's launch form pipes directly into its costume + audio
 * dropdowns. `null` means "no manifest match" or "manifest mapping
 * present but the corresponding prep'd file is missing on disk" — the
 * dashboard treats both cases the same: do not auto-default that field.
 */
export interface AssetsManifestParticipant {
  name: string;
  costumeFile: string | null;
  audioFile: string | null;
}

export interface AssetsManifestResponse {
  participants: AssetsManifestParticipant[];
}

interface AssetsManifestCacheState {
  cache: { value: AssetsManifestResponse; expiresAt: number } | null;
}

/**
 * A handler-thrown error that should render as a specific HTTP
 * status. Anything else thrown out of a handler becomes a `500`.
 */
export class ControlServerError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ControlServerError";
  }
}

interface RouteResult {
  status: number;
  body: unknown;
}

/**
 * Spin up the HTTP control server on `port` (0 ⇒ pick free port).
 * Resolves once `listen` callback has fired and the actual bound
 * port is known.
 */
export function startControlServer(opts: ControlServerOptions): Promise<ControlServerHandle> {
  // Per-process state for the SSO recapture endpoints. Lives on the
  // server handle (closed-over here) so each `startControlServer` call
  // gets its own map — important for the in-process tests.
  const ssoRecaptureSessions: Map<string, SsoRecaptureEntry> = new Map();
  const oauthCaptureSessions: Map<string, OauthCaptureEntry> = new Map();
  const prepAssetsJobs: Map<string, PrepAssetsJob> = new Map();
  const idleTimeout = opts.ssoRecaptureIdleTimeoutMs ?? SSO_RECAPTURE_IDLE_TIMEOUT_MS;
  // Per-server response cache for /assets/manifest. Survives across
  // requests but is scoped to one `startControlServer` call so tests
  // get a clean slate each time. `cache` is mutated in-place by the
  // route handler.
  const assetsManifestState: AssetsManifestCacheState = { cache: null };

  const closeSsoRecaptureSessions = async (): Promise<void> => {
    const entries = Array.from(ssoRecaptureSessions.values());
    ssoRecaptureSessions.clear();
    for (const entry of entries) {
      clearTimeout(entry.idleTimer);
      try {
        await entry.session.close();
      } catch (e) {
        console.warn(
          `[control] failed to close stranded sso recapture ${entry.id}: ${(e as Error).message}`,
        );
      }
    }
    // Tear down stranded OAuth capture sessions alongside SSO ones —
    // they share the same Playwright Chromium pattern and the same
    // shutdown semantics, so it would be confusing to leak one while
    // closing the other.
    const oauthEntries = Array.from(oauthCaptureSessions.values());
    oauthCaptureSessions.clear();
    for (const entry of oauthEntries) {
      clearTimeout(entry.idleTimer);
      try {
        await entry.session.close();
      } catch (e) {
        console.warn(
          `[control] failed to close stranded oauth capture ${entry.id}: ${(e as Error).message}`,
        );
      }
    }
  };

  return new Promise<ControlServerHandle>((resolve, reject) => {
    const server = createServer((req, res) => {
      handleRequest(
        req,
        res,
        opts,
        { ssoRecaptureSessions, oauthCaptureSessions, idleTimeout },
        assetsManifestState,
        prepAssetsJobs,
      ).catch((err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        sendJson(res, 500, { error: `internal error: ${msg}` });
      });
    });
    server.once("error", reject);
    server.listen(opts.port, "127.0.0.1", () => {
      server.off("error", reject);
      const addr = server.address();
      if (addr === null || typeof addr === "string") {
        reject(new Error("control server: unexpected address type from listen()"));
        return;
      }
      const handle: ControlServerHandle = {
        port: addr.port,
        server,
        close: async () => {
          // Always tear down stranded recapture browsers before
          // closing the HTTP listener — otherwise a parent that
          // shuts down mid-recapture leaks a Chrome process.
          await closeSsoRecaptureSessions();
          await new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          });
        },
        closeSsoRecaptureSessions,
      };
      resolve(handle);
    });
  });
}

interface SsoState {
  ssoRecaptureSessions: Map<string, SsoRecaptureEntry>;
  oauthCaptureSessions: Map<string, OauthCaptureEntry>;
  idleTimeout: number;
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
  opts: ControlServerOptions,
  ssoState: SsoState,
  assetsManifestState: AssetsManifestCacheState,
  prepAssetsJobs: Map<string, PrepAssetsJob>,
): Promise<void> {
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const { pathname } = url;
  const method = req.method ?? "GET";

  // `/healthz` is the only path that does NOT require auth — used
  // by readiness probes and `ctl status --help`-style introspection
  // that doesn't yet know the token. Returns the in-flight bot count
  // as a sanity signal but no individual bot detail.
  if (method === "GET" && pathname === "/healthz") {
    const registry = opts.surface.getRegistry();
    sweepStaleEntries(registry);
    const live = countLiveBots(registry);
    sendJson(res, 200, { ok: true, bots: live });
    return;
  }

  if (!authenticate(req, opts.token)) {
    sendJson(res, 401, { error: "unauthorized" });
    return;
  }

  // Sweep done/failed entries that have exceeded the retention window
  // before serving any list/detail call. Doing this on the request
  // path (vs a setInterval) keeps the orchestrator process truly
  // idle when nobody's polling, and is cheap enough at our scale.
  sweepStaleEntries(opts.surface.getRegistry());
  sweepStalePrepAssetsJobs(prepAssetsJobs);

  // SSE streaming route is handled out-of-band because it owns the
  // response lifetime itself (writes its own headers, keeps the
  // connection open, pushes events). All other routes go through the
  // RouteResult-based `route` dispatcher below.
  const prepStreamMatch = /^\/assets\/prep\/([^/]+)\/stream$/.exec(pathname);
  if (prepStreamMatch && method === "GET") {
    const jobId = decodeURIComponent(prepStreamMatch[1]);
    handlePrepAssetsStream(req, res, prepAssetsJobs, jobId);
    return;
  }

  try {
    const result = await route(
      req,
      opts,
      pathname,
      method,
      ssoState,
      assetsManifestState,
      prepAssetsJobs,
      url,
    );
    sendJson(res, result.status, result.body);
  } catch (err) {
    if (err instanceof ControlServerError) {
      sendJson(res, err.status, { error: err.message });
      return;
    }
    if (err instanceof ProfileValidationError) {
      sendJson(res, 400, { error: err.message });
      return;
    }
    if (err instanceof ProfileNotFoundError) {
      sendJson(res, 404, { error: err.message });
      return;
    }
    if (err instanceof ProfileExistsError) {
      sendJson(res, 409, { error: err.message });
      return;
    }
    if (err instanceof SshHostValidationError) {
      sendJson(res, 400, { error: err.message });
      return;
    }
    if (err instanceof SshHostNotFoundError) {
      sendJson(res, 404, { error: err.message });
      return;
    }
    if (err instanceof SshHostExistsError) {
      sendJson(res, 409, { error: err.message });
      return;
    }
    if (err instanceof NotSupportedRemoteError) {
      sendJson(res, 501, { error: err.message });
      return;
    }
    throw err;
  }
}

function authenticate(req: IncomingMessage, expected: string): boolean {
  const supplied = extractBearerToken(req.headers["authorization"]);
  if (supplied === null) return false;
  return tokensMatch(expected, supplied);
}

async function route(
  req: IncomingMessage,
  opts: ControlServerOptions,
  pathname: string,
  method: string,
  ssoState: SsoState,
  assetsManifestState: AssetsManifestCacheState,
  prepAssetsJobs: Map<string, PrepAssetsJob>,
  url: URL,
): Promise<RouteResult> {
  const surface = opts.surface;
  if (method === "GET" && pathname === "/bots") {
    return listBots(surface);
  }
  if (method === "POST" && pathname === "/launch") {
    const body = await readJsonBody(req);
    return launchOne(surface, body);
  }
  if (method === "POST" && pathname === "/launch/multi") {
    const body = await readJsonBody(req);
    return launchMultiRoute(opts, body);
  }
  if (method === "POST" && pathname === "/launch/from-config") {
    const body = await readJsonBody(req);
    return launchFromConfigRoute(opts, body);
  }
  if (method === "POST" && pathname === "/launch/from-config/preview") {
    const body = await readJsonBody(req);
    return previewFromConfigRoute(body);
  }
  if (method === "GET" && pathname === "/assets/manifest") {
    return assetsManifestRoute(opts, assetsManifestState);
  }

  // Prep-assets background job endpoints. SSE stream is handled
  // out-of-band in `handleRequest`.
  if (method === "POST" && pathname === "/assets/prep") {
    const body = await readJsonBody(req);
    return prepAssetsStartRoute(opts, prepAssetsJobs, body);
  }
  const prepJobMatch = /^\/assets\/prep\/([^/]+)$/.exec(pathname);
  if (prepJobMatch && method === "GET") {
    const jobId = decodeURIComponent(prepJobMatch[1]);
    return prepAssetsStatusRoute(prepAssetsJobs, jobId);
  }
  const prepCancelMatch = /^\/assets\/prep\/([^/]+)$/.exec(pathname);
  if (prepCancelMatch && method === "DELETE") {
    const jobId = decodeURIComponent(prepCancelMatch[1]);
    return prepAssetsForgetRoute(prepAssetsJobs, jobId);
  }

  // ──────────────────────────────────────────────────────────────────
  // OAuth storage-state capture endpoints (parallel to HCL SSO recapture
  // but for Google OAuth targets like app.videocall.rs). Sessions live
  // at <runDir>/auth/<label>.json. HCL SSO state (hcl-sso.json) is
  // excluded from the listing — it is owned by the SSO recapture flow.
  // ──────────────────────────────────────────────────────────────────
  if (method === "GET" && pathname === "/oauth/sessions") {
    return oauthSessionsRoute(opts);
  }
  if (method === "POST" && pathname === "/oauth/capture") {
    const body = await readJsonBody(req);
    return oauthCaptureStartRoute(opts, ssoState, body);
  }
  const oauthCapturePath = /^\/oauth\/capture\/([^/]+)(?:\/(complete))?$/.exec(pathname);
  if (oauthCapturePath) {
    const sessionId = decodeURIComponent(oauthCapturePath[1]);
    const sub = oauthCapturePath[2];
    if (sub === "complete" && method === "POST") {
      return oauthCaptureCompleteRoute(opts, ssoState, sessionId);
    }
    if (sub === undefined && method === "DELETE") {
      return oauthCaptureCancelRoute(ssoState, sessionId);
    }
  }
  const oauthSessionPath = /^\/oauth\/sessions\/([^/]+)$/.exec(pathname);
  if (oauthSessionPath && method === "DELETE") {
    const label = decodeURIComponent(oauthSessionPath[1]);
    return oauthSessionDeleteRoute(opts, label);
  }

  // ──────────────────────────────────────────────────────────────────
  // HCL SSO / VPN status endpoints (feat/bots-app-dashboard-sso)
  // ──────────────────────────────────────────────────────────────────
  if (method === "GET" && pathname === "/sso/vpn-status") {
    return vpnStatusRoute(opts);
  }
  if (method === "GET" && pathname === "/sso/status") {
    return ssoStatusRoute(opts);
  }
  if (method === "POST" && pathname === "/sso/recapture") {
    const body = await readJsonBody(req);
    return ssoRecaptureStartRoute(opts, ssoState, body);
  }
  const recapturePath = /^\/sso\/recapture\/([^/]+)(?:\/(complete))?$/.exec(pathname);
  if (recapturePath) {
    const sessionId = decodeURIComponent(recapturePath[1]);
    const sub = recapturePath[2];
    if (sub === "complete" && method === "POST") {
      return ssoRecaptureCompleteRoute(opts, ssoState, sessionId);
    }
    if (sub === undefined && method === "DELETE") {
      return ssoRecaptureCancelRoute(ssoState, sessionId);
    }
  }

  if (pathname === "/profiles") {
    if (method === "GET") return listProfilesRoute(opts);
    if (method === "POST") {
      const body = await readJsonBody(req);
      return saveProfileRoute(opts, body);
    }
  }
  const profilePath = /^\/profiles\/([^/]+)(?:\/([^/]+))?$/.exec(pathname);
  if (profilePath) {
    const name = decodeURIComponent(profilePath[1]);
    const sub = profilePath[2];
    if (sub === undefined) {
      if (method === "GET") return getProfileRoute(opts, name);
      if (method === "DELETE") return deleteProfileRoute(opts, name);
    } else if (sub === "launch" && method === "POST") {
      return launchProfileRoute(opts, name);
    } else if (sub === "rename" && method === "POST") {
      const body = await readJsonBody(req);
      return renameProfileRoute(opts, name, body);
    }
  }

  // ──────────────────────────────────────────────────────────────────
  // SSH host registry endpoints. CRUD + a `POST /hosts/:label/test`
  // probe that runs `ssh -o ConnectTimeout=5 ... 'echo … && uname -a'`
  // so the operator can sanity-check a registered host without leaving
  // the dashboard. Persistence lives at `<runDir>/hosts.json` (mode
  // 0o600). See `./ssh-hosts.ts` for the wire shape.
  // ──────────────────────────────────────────────────────────────────
  if (pathname === "/hosts") {
    if (method === "GET") return listHostsRoute(opts);
    if (method === "POST") {
      const body = await readJsonBody(req);
      return addHostRoute(opts, body);
    }
  }
  // `/hosts/preview` — preview the SSH command for an UNSAVED host
  // (used by the Add / Edit dialog's live preview). The host config
  // arrives in the request body and is validated as if it were being
  // added/edited; nothing is persisted.
  if (pathname === "/hosts/preview" && method === "POST") {
    const body = await readJsonBody(req);
    return previewHostRoute(opts, body);
  }
  const hostPath = /^\/hosts\/([^/]+)(?:\/([^/]+))?$/.exec(pathname);
  if (hostPath) {
    const label = decodeURIComponent(hostPath[1]);
    const sub = hostPath[2];
    if (sub === undefined) {
      if (method === "PUT") {
        const body = await readJsonBody(req);
        return updateHostRoute(opts, label, body);
      }
      if (method === "DELETE") return removeHostRoute(opts, label);
    } else if (sub === "test" && method === "POST") {
      return testHostRoute(opts, label);
    } else if (sub === "preview-launch" && method === "POST") {
      const body = await readJsonBody(req);
      return previewLaunchRoute(opts, label, body);
    }
  }

  const botPath = /^\/bots\/([^/]+)(?:\/([^/]+))?$/.exec(pathname);
  if (botPath) {
    const botId = decodeURIComponent(botPath[1]);
    const sub = botPath[2];
    if (sub === undefined) {
      if (method === "GET") return getOneBot(surface, botId);
      if (method === "DELETE") return killBot(surface, botId);
    } else if (sub === "log" && method === "GET") {
      return botLogRoute(surface, botId, url);
    } else {
      const body = await readJsonBody(req);
      if (method === "POST" && sub === "leave") return leaveBot(surface, botId);
      if (method === "POST" && sub === "ttl") return applyTtl(surface, botId, body);
      if (method === "POST" && sub === "network") return changeNetwork(surface, botId, body);
      if (method === "POST" && sub === "mute") return mute(surface, botId, body);
      if (method === "POST" && sub === "video") return video(surface, botId, body);
      if (method === "POST" && sub === "share") return share(surface, botId, body);
      if (method === "POST" && sub === "duplicate") return duplicate(surface, botId, body);
    }
  }

  return { status: 404, body: { error: `no route for ${method} ${pathname}` } };
}

function requireRunDir(opts: ControlServerOptions): string {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "profiles unavailable: control server was started without a runDir",
    );
  }
  return opts.runDir;
}

async function listProfilesRoute(opts: ControlServerOptions): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const profiles = await listProfiles(runDir);
  return { status: 200, body: { profiles } };
}

async function getProfileRoute(opts: ControlServerOptions, name: string): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const profile = await readProfile(runDir, name);
  return { status: 200, body: profile };
}

async function deleteProfileRoute(opts: ControlServerOptions, name: string): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  await deleteProfile(runDir, name);
  return { status: 200, body: { name, deleted: true } };
}

async function saveProfileRoute(
  opts: ControlServerOptions,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const name = body.name;
  if (typeof name !== "string" || name === "") {
    throw new ControlServerError(400, '"name" must be a non-empty string');
  }
  const source = body.source;
  let bots: ProfileBotSpec[];
  if (source === "current") {
    bots = snapshotCurrentBotsForProfile(opts.surface);
    if (bots.length === 0) {
      throw new ControlServerError(
        400,
        "no bots to snapshot — launch some first, then save the profile",
      );
    }
  } else if (
    source != null &&
    typeof source === "object" &&
    !Array.isArray(source) &&
    Array.isArray((source as { bots?: unknown }).bots)
  ) {
    bots = (source as { bots: unknown[] }).bots.map((entry, idx) =>
      validateBotSpecForSave(entry, `source.bots[${idx}]`),
    );
    if (bots.length === 0) {
      throw new ControlServerError(400, "source.bots must contain at least one bot");
    }
  } else {
    throw new ControlServerError(
      400,
      'source must be "current" or an object { bots: BotLaunchSpec[] }',
    );
  }
  const profile = await saveProfile(runDir, name, bots);
  return { status: 201, body: profile };
}

async function renameProfileRoute(
  opts: ControlServerOptions,
  oldName: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const newName = body.newName;
  if (typeof newName !== "string" || newName === "") {
    throw new ControlServerError(400, '"newName" must be a non-empty string');
  }
  // The pattern + length cap are enforced by `profilePath` inside
  // `renameProfile`, which throws `ProfileValidationError` on bad
  // input. We surface that as a 400 via the top-level catch in
  // `handleRequest`.
  const profile = await renameProfile(runDir, oldName, newName);
  return { status: 200, body: profile };
}

async function launchProfileRoute(opts: ControlServerOptions, name: string): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const profile = await readProfile(runDir, name);
  const botIds: string[] = [];
  for (const bot of profile.bots) {
    let ttl: Ttl;
    try {
      ttl = parseDuration(bot.ttl);
    } catch (e) {
      throw new ControlServerError(400, `invalid ttl in profile: ${(e as Error).message}`);
    }
    const spec: LaunchSpec = {
      meetingURL: bot.meetingURL,
      participant: bot.participant,
      displayName: bot.displayName,
      ttl,
      headless: bot.headless,
      network: bot.network,
      authBackend: bot.authBackend,
      storageStateFile: bot.storageStateFile,
      // Profiles persisted before the runLocation extension are
      // re-dispatched locally — that matches the pre-extension
      // behavior (the orchestrator's launchOne defaults to
      // `{ kind: "local" }` on a missing field). Profiles captured
      // after the extension carry the original host verbatim so an
      // SSH-hosted bot resumes on the same registered host.
      runLocation: bot.runLocation ?? { kind: "local" },
    };
    const id = await opts.surface.launchOne(spec);
    botIds.push(id);
  }
  return { status: 202, body: { name, botIds } };
}

/**
 * Take a point-in-time snapshot of every bot currently live or
 * recently-finished in the registry, projecting each one's `task`
 * back into a `ProfileBotSpec` shape that can be re-launched. Strips
 * runtime-only fields (manifests, runDir, sso state) so the resulting
 * profile is portable across runs.
 */
function snapshotCurrentBotsForProfile(surface: OrchestratorControlSurface): ProfileBotSpec[] {
  const out: ProfileBotSpec[] = [];
  for (const entry of surface.getRegistry().values()) {
    const t = entry.task;
    out.push({
      meetingURL: t.meetingURL,
      participant: t.participant,
      displayName: t.displayName,
      ttl: formatDuration(t.ttl),
      headless: t.headless,
      network: t.network ?? "none",
      authBackend: t.authBackend,
      storageStateFile: t.storageStateFile ?? undefined,
      // Capture where this bot is currently running so the profile
      // replays each bot on the same host on the next launch. Bots
      // running locally serialize as `{ kind: "local" }`; SSH bots
      // serialize with the registered `hostLabel`.
      runLocation: entry.host,
    });
  }
  return out;
}

function validateBotSpecForSave(entry: unknown, where: string): ProfileBotSpec {
  if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
    throw new ControlServerError(400, `${where} must be an object`);
  }
  const o = entry as Record<string, unknown>;
  if (typeof o.meetingURL !== "string") {
    throw new ControlServerError(400, `${where}.meetingURL must be a string`);
  }
  if (typeof o.participant !== "string") {
    throw new ControlServerError(400, `${where}.participant must be a string`);
  }
  if (typeof o.ttl !== "string") {
    throw new ControlServerError(400, `${where}.ttl must be a string`);
  }
  if (typeof o.network !== "string") {
    throw new ControlServerError(400, `${where}.network must be a string`);
  }
  if (typeof o.headless !== "boolean") {
    throw new ControlServerError(400, `${where}.headless must be a boolean`);
  }
  const auth = o.authBackend;
  if (auth !== "jwt" && auth !== "storage-state" && auth !== "none") {
    throw new ControlServerError(
      400,
      `${where}.authBackend must be "jwt", "storage-state", or "none"`,
    );
  }
  const displayName =
    o.displayName === undefined
      ? undefined
      : typeof o.displayName === "string"
        ? o.displayName
        : (() => {
            throw new ControlServerError(400, `${where}.displayName must be a string`);
          })();
  const storageStateFile =
    o.storageStateFile === undefined
      ? undefined
      : typeof o.storageStateFile === "string"
        ? o.storageStateFile
        : (() => {
            throw new ControlServerError(400, `${where}.storageStateFile must be a string`);
          })();
  // Optional structured run-location. Validate strictly when present
  // (`{ kind: "local" }` or `{ kind: "ssh", hostLabel: <non-empty> }`)
  // and pass `undefined` through silently when the caller's bot spec
  // predates the field — the launch route fills in a local default.
  const runLocation = parseRunLocationFromSaveBody(o.runLocation, `${where}.runLocation`);
  return {
    meetingURL: o.meetingURL,
    participant: o.participant,
    displayName,
    ttl: o.ttl,
    headless: o.headless,
    network: o.network,
    authBackend: auth,
    storageStateFile,
    runLocation,
  };
}

function parseRunLocationFromSaveBody(raw: unknown, where: string): ProfileBotSpec["runLocation"] {
  if (raw === undefined || raw === null) return undefined;
  if (typeof raw !== "object" || Array.isArray(raw)) {
    throw new ControlServerError(400, `${where} must be an object`);
  }
  const o = raw as Record<string, unknown>;
  if (o.kind === "local") {
    return { kind: "local" };
  }
  if (o.kind === "ssh") {
    if (typeof o.hostLabel !== "string" || o.hostLabel.trim() === "") {
      throw new ControlServerError(
        400,
        `${where}.hostLabel must be a non-empty string when kind="ssh"`,
      );
    }
    return { kind: "ssh", hostLabel: o.hostLabel };
  }
  throw new ControlServerError(400, `${where}.kind must be "local" or "ssh"`);
}

function listBots(surface: OrchestratorControlSurface): RouteResult {
  const now = Date.now();
  const snapshots: BotSnapshot[] = [];
  for (const entry of surface.getRegistry().values()) {
    snapshots.push(snapshotEntry(entry, now));
  }
  return { status: 200, body: { bots: snapshots } };
}

function getOneBot(surface: OrchestratorControlSurface, botId: string): RouteResult {
  const entry = surface.getRegistry().get(botId);
  if (entry === undefined) {
    throw new ControlServerError(404, `bot ${botId} not found`);
  }
  return { status: 200, body: snapshotEntry(entry) };
}

async function leaveBot(surface: OrchestratorControlSurface, botId: string): Promise<RouteResult> {
  requireBot(surface, botId);
  await surface.triggerLeave(botId);
  return { status: 202, body: { botId, action: "leave" } };
}

async function killBot(surface: OrchestratorControlSurface, botId: string): Promise<RouteResult> {
  requireBot(surface, botId);
  await surface.forceKill(botId);
  return { status: 202, body: { botId, action: "kill" } };
}

function applyTtl(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): RouteResult {
  requireBot(surface, botId);
  const setValue = typeof body.ttl === "string" ? body.ttl : null;
  const extendValue = typeof body.extendBy === "string" ? body.extendBy : null;
  if (setValue !== null && extendValue !== null) {
    throw new ControlServerError(400, 'specify exactly one of "ttl" or "extendBy"');
  }
  if (setValue === null && extendValue === null) {
    throw new ControlServerError(400, 'specify one of "ttl" (set) or "extendBy" (extend)');
  }

  let newTtl: Ttl;
  if (setValue !== null) {
    try {
      newTtl = parseDuration(setValue);
    } catch (e) {
      throw new ControlServerError(400, (e as Error).message);
    }
  } else {
    let extra: Ttl;
    try {
      extra = parseDuration(extendValue as string);
    } catch (e) {
      throw new ControlServerError(400, (e as Error).message);
    }
    if (extra === "infinite") {
      newTtl = "infinite";
    } else {
      const entry = surface.getRegistry().get(botId)!;
      if (entry.ttlDeadline === null) {
        // already infinite — extending an infinite ttl is a no-op
        newTtl = "infinite";
      } else {
        const remaining = Math.max(0, entry.ttlDeadline - Date.now());
        newTtl = remaining + extra;
      }
    }
  }

  surface.applyTtl(botId, newTtl);
  return { status: 200, body: { botId, ttl: formatDuration(newTtl) } };
}

async function changeNetwork(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  requireBot(surface, botId);
  const network = body.network;
  if (typeof network !== "string") {
    throw new ControlServerError(400, '"network" must be a string');
  }
  if (!NETSIM_PRESETS.includes(network)) {
    throw new ControlServerError(
      400,
      `"network" must be one of: ${NETSIM_PRESETS.join(", ")} (got "${network}")`,
    );
  }
  await surface.changeNetwork(botId, network);
  return { status: 202, body: { botId, network, note: "reconnecting bot to apply new netsim" } };
}

async function mute(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  requireBot(surface, botId);
  if (typeof body.mic !== "boolean") {
    throw new ControlServerError(400, '"mic" must be a boolean (true=mute, false=unmute)');
  }
  await surface.setMicMuted(botId, body.mic);
  return { status: 200, body: { botId, mic: body.mic } };
}

async function video(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  requireBot(surface, botId);
  if (typeof body.camera !== "boolean") {
    throw new ControlServerError(
      400,
      '"camera" must be a boolean (true=camera off, false=camera on)',
    );
  }
  await surface.setCameraOff(botId, body.camera);
  return { status: 200, body: { botId, camera: body.camera } };
}

async function share(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  requireBot(surface, botId);
  if (typeof body.share !== "boolean") {
    throw new ControlServerError(
      400,
      '"share" must be a boolean (true=start sharing, false=stop sharing)',
    );
  }
  await surface.setScreenShare(botId, body.share);
  return { status: 200, body: { botId, share: body.share } };
}

async function launchOne(
  surface: OrchestratorControlSurface,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  // Validate each field one at a time so we can return precise error
  // messages. The dashboard's client-side validation runs the same
  // checks first, but the server is the source of truth.
  const meetingURL = body.meetingURL;
  if (typeof meetingURL !== "string" || meetingURL === "") {
    throw new ControlServerError(400, '"meetingURL" must be a non-empty string');
  }
  let url: URL;
  try {
    url = new URL(meetingURL);
  } catch {
    throw new ControlServerError(400, `"meetingURL" is not a valid URL`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new ControlServerError(400, `"meetingURL" must use http or https`);
  }
  const participant = body.participant;
  if (typeof participant !== "string" || participant === "") {
    throw new ControlServerError(400, '"participant" must be a non-empty string');
  }
  if (!/^[A-Za-z0-9._@+-]+$/.test(participant)) {
    throw new ControlServerError(400, '"participant" contains invalid characters');
  }
  const displayName = body.displayName;
  if (displayName !== undefined && typeof displayName !== "string") {
    throw new ControlServerError(400, '"displayName" must be a string when provided');
  }
  const ttlRaw = body.ttl;
  if (typeof ttlRaw !== "string") {
    throw new ControlServerError(400, '"ttl" must be a string');
  }
  let ttl: Ttl;
  try {
    ttl = parseDuration(ttlRaw);
  } catch (e) {
    throw new ControlServerError(400, (e as Error).message);
  }
  const headless = body.headless;
  if (typeof headless !== "boolean") {
    throw new ControlServerError(400, '"headless" must be a boolean');
  }
  const network = body.network;
  if (typeof network !== "string") {
    throw new ControlServerError(400, '"network" must be a string');
  }
  if (!NETSIM_PRESETS.includes(network)) {
    throw new ControlServerError(
      400,
      `"network" must be one of: ${NETSIM_PRESETS.join(", ")} (got "${network}")`,
    );
  }
  const authBackend = body.authBackend;
  if (authBackend !== "jwt" && authBackend !== "storage-state" && authBackend !== "none") {
    throw new ControlServerError(400, '"authBackend" must be "jwt", "storage-state", or "none"');
  }
  const storageStateFile = body.storageStateFile;
  if (storageStateFile !== undefined && typeof storageStateFile !== "string") {
    throw new ControlServerError(400, '"storageStateFile" must be a string when provided');
  }
  if (authBackend === "storage-state" && (!storageStateFile || storageStateFile === "")) {
    throw new ControlServerError(
      400,
      '"storageStateFile" is required when authBackend === "storage-state"',
    );
  }
  const ssoStateFile = body.ssoStateFile;
  if (ssoStateFile !== undefined && typeof ssoStateFile !== "string") {
    throw new ControlServerError(400, '"ssoStateFile" must be a string when provided');
  }
  // Costume / audio overrides: the dashboard's launch form picks a
  // basename from its `/api/assets/{costumes,audio}` listing. Validate
  // up-front so a malicious or fat-fingered value (`/etc/passwd`,
  // `../../somewhere.y4m`) can't escape the `<runDir>/{costumes,audio}/`
  // directory once the orchestrator composes the absolute path.
  //
  // The literal sentinel `"default"` is accepted for symmetry with the
  // dashboard's Select widget ("Default fake pattern") — it's
  // semantically equivalent to omitting the field.
  const costume = body.costume;
  if (costume !== undefined && typeof costume !== "string") {
    throw new ControlServerError(400, '"costume" must be a string when provided');
  }
  if (
    typeof costume === "string" &&
    costume !== "" &&
    costume !== "default" &&
    !ASSET_FILENAME_PATTERN.test(costume)
  ) {
    throw new ControlServerError(
      400,
      `"costume" must match ${ASSET_FILENAME_PATTERN.source} (got "${costume}")`,
    );
  }
  const audio = body.audio;
  if (audio !== undefined && typeof audio !== "string") {
    throw new ControlServerError(400, '"audio" must be a string when provided');
  }
  if (
    typeof audio === "string" &&
    audio !== "" &&
    audio !== "default" &&
    !ASSET_FILENAME_PATTERN.test(audio)
  ) {
    throw new ControlServerError(
      400,
      `"audio" must match ${ASSET_FILENAME_PATTERN.source} (got "${audio}")`,
    );
  }
  // `runLocation` carries the operator's "where does this bot's
  // Chrome run" pick. The legacy shape was the bare string `"local"`
  // / `"future-ssh"`; the SSH PR introduces the structured form
  // `{ kind: "local" }` / `{ kind: "ssh", hostLabel }`. We accept
  // either shape for back-compat and normalize to the structured one.
  const runLocation = parseRunLocationField(body.runLocation);
  const spec: LaunchSpec = {
    meetingURL,
    participant,
    displayName: displayName as string | undefined,
    ttl,
    headless,
    network,
    authBackend,
    storageStateFile: storageStateFile as string | undefined,
    ssoStateFile: ssoStateFile as string | undefined,
    costume: costume as string | undefined,
    audio: audio as string | undefined,
    runLocation,
  };
  const newId = await surface.launchOne(spec);
  return { status: 201, body: { botId: newId } };
}

/**
 * Parse the `runLocation` field accepted by `POST /launch` (and the
 * multi-launch variants). Accepts:
 *
 *   - undefined / "local" / { kind: "local" }  → { kind: "local" }
 *   - { kind: "ssh", hostLabel: "<label>" }    → { kind: "ssh", … }
 *
 * The pre-SSH dashboard used the bare strings `"future-vm"` /
 * `"future-ssh"` / `"future-docker"` as placeholders; those are now
 * rejected explicitly so a regression on the UI side surfaces loudly
 * instead of silently downgrading to local.
 */
export function parseRunLocationField(
  raw: unknown,
): { kind: "local" } | { kind: "ssh"; hostLabel: string } {
  if (raw === undefined || raw === null) return { kind: "local" };
  if (raw === "local") return { kind: "local" };
  if (typeof raw === "string") {
    if (raw === "future-vm" || raw === "future-docker") {
      throw new ControlServerError(
        400,
        `runLocation "${raw}" is not wired in this release; see discussion #793`,
      );
    }
    if (raw === "future-ssh") {
      throw new ControlServerError(
        400,
        'runLocation "future-ssh" is the legacy placeholder; pass { kind: "ssh", hostLabel } instead',
      );
    }
    throw new ControlServerError(400, `unknown runLocation string "${raw}"`);
  }
  if (typeof raw === "object" && !Array.isArray(raw)) {
    const o = raw as Record<string, unknown>;
    if (o.kind === "local") return { kind: "local" };
    if (o.kind === "ssh") {
      const hostLabel = o.hostLabel;
      if (typeof hostLabel !== "string" || hostLabel === "") {
        throw new ControlServerError(
          400,
          'runLocation.kind="ssh" requires a non-empty "hostLabel"',
        );
      }
      return { kind: "ssh", hostLabel };
    }
    throw new ControlServerError(
      400,
      `runLocation.kind must be "local" or "ssh" (got ${JSON.stringify(o.kind)})`,
    );
  }
  throw new ControlServerError(400, "runLocation must be a string or { kind, hostLabel }");
}

async function duplicate(
  surface: OrchestratorControlSurface,
  botId: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  requireBot(surface, botId);
  const overrides: { participant?: string; ttl?: Ttl; network?: string } = {};
  if (body.participant !== undefined) {
    if (typeof body.participant !== "string" || body.participant === "") {
      throw new ControlServerError(400, '"participant" must be a non-empty string');
    }
    overrides.participant = body.participant;
  }
  if (body.ttl !== undefined) {
    if (typeof body.ttl !== "string") {
      throw new ControlServerError(400, '"ttl" must be a string');
    }
    try {
      overrides.ttl = parseDuration(body.ttl);
    } catch (e) {
      throw new ControlServerError(400, (e as Error).message);
    }
  }
  if (body.network !== undefined) {
    if (typeof body.network !== "string") {
      throw new ControlServerError(400, '"network" must be a string');
    }
    if (!NETSIM_PRESETS.includes(body.network)) {
      throw new ControlServerError(
        400,
        `"network" must be one of: ${NETSIM_PRESETS.join(", ")} (got "${body.network}")`,
      );
    }
    overrides.network = body.network;
  }
  const newId = await surface.duplicateBot(botId, overrides);
  return { status: 201, body: { botId: newId } };
}

function requireBot(surface: OrchestratorControlSurface, botId: string): BotRegistryEntry {
  const entry = surface.getRegistry().get(botId);
  if (entry === undefined) {
    throw new ControlServerError(404, `bot ${botId} not found`);
  }
  return entry;
}

// ──────────────────────────────────────────────────────────────────────
// HCL SSO / VPN status route handlers
// ──────────────────────────────────────────────────────────────────────

/**
 * Best-effort fetch of {@link DEFAULT_VPN_CHECK_URL} (or the
 * `VPN_CHECK_URL` env override) with a 5s timeout. Returns one of:
 *
 *   `{ status: "up", responseTimeMs }`   — TCP+TLS reachable, server responded.
 *   `{ status: "down", error }`          — timeout / DNS / connect / TLS / 5xx.
 *
 * A 401 is treated as "up" — it means the VPN is reachable, the gated
 * site responded, and the only reason the response wasn't 200 is the
 * lack of a session cookie (which is expected here: the dashboard does
 * not inject one). Conflating 401 with "down" would mask the actual
 * VPN status and trigger spurious "VPN unreachable" UI banners.
 */
async function vpnStatusRoute(opts: ControlServerOptions): Promise<RouteResult> {
  const target = process.env.VPN_CHECK_URL ?? DEFAULT_VPN_CHECK_URL;
  const fetchImpl = opts.vpnFetch ?? globalThis.fetch;
  const checkedAt = Date.now();
  if (typeof fetchImpl !== "function") {
    return {
      status: 200,
      body: {
        status: "down" as const,
        checkedAt,
        error: "fetch unavailable in this runtime",
      },
    };
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 5000);
  const t0 = Date.now();
  try {
    const res = await fetchImpl(target, {
      method: "GET",
      redirect: "manual",
      signal: controller.signal,
    });
    const responseTimeMs = Date.now() - t0;
    // 1xx/2xx/3xx/4xx = reachable. 5xx = the upstream is broken even
    // though the VPN itself is up; we still classify that as "down"
    // for the operator's purpose ("can I usefully launch a bot here?")
    // — same as the legacy curl one-liner the team was using.
    if (res.status >= 500) {
      return {
        status: 200,
        body: {
          status: "down" as const,
          checkedAt,
          error: `HTTP ${res.status}`,
          responseTimeMs,
        },
      };
    }
    return {
      status: 200,
      body: {
        status: "up" as const,
        checkedAt,
        responseTimeMs,
        httpStatus: res.status,
      },
    };
  } catch (err) {
    const msg = (err as Error).message ?? String(err);
    let reason = msg;
    if ((err as { name?: string }).name === "AbortError") reason = "timeout";
    else if (msg.includes("ENOTFOUND")) reason = "DNS lookup failed (ENOTFOUND)";
    else if (msg.includes("ECONNREFUSED")) reason = "connection refused";
    else if (/tls|certificate/i.test(msg)) reason = `TLS error: ${msg}`;
    return {
      status: 200,
      body: { status: "down" as const, checkedAt, error: reason },
    };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Report SSO storage-state file status by inspecting the conventional
 * path under `<runDir>/auth/hcl-sso.json`. We deliberately stay out of
 * the file contents — cookies have opaque expiry semantics and trying
 * to predict their validity from JSON shape leads to false positives.
 * mtime is the closest proxy available.
 */
function ssoStatusRoute(opts: ControlServerOptions): RouteResult {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "sso status unavailable: control server was started without a runDir",
    );
  }
  const filePath = defaultSsoStatePath(opts.runDir);
  try {
    const st = statSync(filePath);
    const ageHours = (Date.now() - st.mtimeMs) / (1000 * 60 * 60);
    return {
      status: 200,
      body: {
        filePath,
        exists: true,
        capturedAt: st.mtimeMs,
        ageHours,
        size: st.size,
      },
    };
  } catch {
    return {
      status: 200,
      body: {
        filePath,
        exists: false,
        capturedAt: null,
        ageHours: null,
        size: null,
      },
    };
  }
}

async function ssoRecaptureStartRoute(
  opts: ControlServerOptions,
  ssoState: SsoState,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "sso recapture unavailable: control server was started without a runDir",
    );
  }
  let startUrl = DEFAULT_SSO_START_URL;
  if (body.startUrl !== undefined) {
    if (typeof body.startUrl !== "string") {
      throw new ControlServerError(400, '"startUrl" must be a string when provided');
    }
    try {
      const u = new URL(body.startUrl);
      if (u.protocol !== "http:" && u.protocol !== "https:") {
        throw new Error("only http(s) URLs are accepted");
      }
    } catch (e) {
      throw new ControlServerError(400, `invalid startUrl: ${(e as Error).message}`);
    }
    startUrl = body.startUrl;
  }
  const factory = opts.ssoCaptureFactory ?? openSsoCaptureBrowser;
  const sessionId = randomUUID();
  let session: SsoCaptureSession;
  try {
    session = await factory({ startUrl });
  } catch (e) {
    throw new ControlServerError(
      500,
      `sso recapture: browser launch failed: ${(e as Error).message}`,
    );
  }
  const startedAt = Date.now();
  const idleTimer = setTimeout(() => {
    const entry = ssoState.ssoRecaptureSessions.get(sessionId);
    if (entry === undefined) return;
    ssoState.ssoRecaptureSessions.delete(sessionId);
    void entry.session.close().catch((err: unknown) => {
      console.warn(
        `[control] idle-timeout teardown of sso recapture ${sessionId} failed: ${
          (err as Error).message
        }`,
      );
    });
    console.log(
      `[control] sso recapture ${sessionId} auto-cancelled after idle timeout (${ssoState.idleTimeout}ms)`,
    );
  }, ssoState.idleTimeout);
  // Detach the timer from keeping the event loop alive — the parent
  // process should be allowed to exit even with a pending recapture.
  if (typeof idleTimer.unref === "function") idleTimer.unref();
  ssoState.ssoRecaptureSessions.set(sessionId, {
    id: sessionId,
    startUrl,
    startedAt,
    session,
    idleTimer,
  });
  return {
    status: 201,
    body: { recaptureSessionId: sessionId, startUrl, startedAt },
  };
}

async function ssoRecaptureCompleteRoute(
  opts: ControlServerOptions,
  ssoState: SsoState,
  sessionId: string,
): Promise<RouteResult> {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "sso recapture unavailable: control server was started without a runDir",
    );
  }
  const entry = ssoState.ssoRecaptureSessions.get(sessionId);
  if (entry === undefined) {
    throw new ControlServerError(404, `sso recapture session ${sessionId} not found`);
  }
  ssoState.ssoRecaptureSessions.delete(sessionId);
  clearTimeout(entry.idleTimer);
  const outPath = defaultSsoStatePath(opts.runDir);
  try {
    await entry.session.saveAndClose(outPath);
  } catch (e) {
    // Save failed (e.g. operator closed the window first). Make a
    // best-effort to tear down anyway so we don't leak a half-dead
    // browser, then surface a 500.
    await entry.session.close().catch(() => {});
    throw new ControlServerError(500, `sso recapture save failed: ${(e as Error).message}`);
  }
  return ssoStatusRoute(opts);
}

function ssoRecaptureCancelRoute(ssoState: SsoState, sessionId: string): Promise<RouteResult> {
  const entry = ssoState.ssoRecaptureSessions.get(sessionId);
  if (entry === undefined) {
    throw new ControlServerError(404, `sso recapture session ${sessionId} not found`);
  }
  ssoState.ssoRecaptureSessions.delete(sessionId);
  clearTimeout(entry.idleTimer);
  return entry.session
    .close()
    .then(() => ({ status: 200, body: { recaptureSessionId: sessionId, cancelled: true } }));
}

// ──────────────────────────────────────────────────────────────────────
// /assets/manifest — participant → costume/audio mapping for the
// dashboard launch form's auto-default behavior. See
// {@link AssetsManifestResponse} for the wire shape.
// ──────────────────────────────────────────────────────────────────────

/**
 * Build (or reuse the cached) participant → asset mapping. Cache TTL
 * is {@link ASSETS_MANIFEST_CACHE_MS}; on a miss we re-parse the
 * manifest YAML and re-`existsSync` every candidate file. Both
 * `runDir` and `manifestPath` are optional in the server options —
 * when either is missing we return an empty `participants` array
 * (fail-soft, matches the CLI's behavior when the manifest is absent).
 *
 * Per-participant rules:
 *   - participant not in manifest        → costumeFile=null, audioFile=null
 *   - manifest has no costume_dir         → costumeFile=null
 *   - manifest has costume_dir but the
 *     `<runDir>/costumes/<name>.y4m` file
 *     does NOT exist                      → costumeFile=null
 *   - `<runDir>/audio/<name>.wav` exists  → audioFile=`<name>.wav`
 *   - `<runDir>/audio/<name>.wav` missing → audioFile=null
 *
 * Filenames are basenames only — the dashboard's costume/audio Select
 * options use basenames too, so the form can match by string equality.
 */
function assetsManifestRoute(
  opts: ControlServerOptions,
  state: AssetsManifestCacheState,
): RouteResult {
  const now = Date.now();
  if (state.cache !== null && state.cache.expiresAt > now) {
    return { status: 200, body: state.cache.value };
  }
  const value = computeAssetsManifest(opts);
  state.cache = { value, expiresAt: now + ASSETS_MANIFEST_CACHE_MS };
  return { status: 200, body: value };
}

function computeAssetsManifest(opts: ControlServerOptions): AssetsManifestResponse {
  // Fail-soft: if either the manifest or the runDir is missing we just
  // hand back an empty list. The dashboard's auto-match logic skips
  // when participants is empty, so the operator's manual selections
  // remain in charge.
  if (!opts.manifestPath || !opts.runDir) {
    return { participants: [] };
  }
  if (!existsSync(opts.manifestPath)) {
    return { participants: [] };
  }
  let manifest: Manifest;
  try {
    manifest = loadManifest(opts.manifestPath).manifest;
  } catch {
    // Malformed manifest — same fail-soft response. The launch form
    // surfaces this as "no auto-match" rather than a hard error
    // because the operator can still manually pick assets.
    return { participants: [] };
  }
  const costumesDir = join(opts.runDir, "costumes");
  const audioDir = join(opts.runDir, "audio");
  const participants: AssetsManifestParticipant[] = manifest.participants.map((p) => {
    const costumeName = costumeNameForParticipant(manifest, p.name);
    let costumeFile: string | null = null;
    if (costumeName) {
      const candidate = `${costumeName}.y4m`;
      if (existsSync(join(costumesDir, candidate))) {
        costumeFile = candidate;
      }
    }
    const audioCandidate = `${p.name}.wav`;
    const audioFile = existsSync(join(audioDir, audioCandidate)) ? audioCandidate : null;
    return { name: p.name, costumeFile, audioFile };
  });
  return { participants };
}

// ──────────────────────────────────────────────────────────────────────
// Multi-launch + from-config routes. Mirror the CLI's `bots-app gen`
// (random pick) and `bots-app run --users N` (first-N pick) flows so
// dashboard operators can spawn a fleet from one click instead of N
// invocations of the single-bot launch form.
// ──────────────────────────────────────────────────────────────────────

/**
 * Pick `count` participants from the manifest using the requested
 * mode. Re-implements the legacy CLI's first-N + random selection
 * exactly so a given (`seed`, `count`, `includeObservers`, manifest)
 * tuple produces the same names through either entry point.
 *
 * Throws when:
 *   - `mode === "first-n"` and the manifest has fewer than `count`
 *     named participants
 *   - `mode === "random"`  and the eligible pool (costumed-only by
 *     default, all when `includeObservers === true`) has fewer than
 *     `count` rows
 */
export function pickParticipantsForMultiLaunch(args: {
  manifest: Manifest;
  mode: "first-n" | "random";
  count: number;
  seed?: number;
  includeObservers?: boolean;
}): string[] {
  if (args.mode === "first-n") {
    if (args.count > args.manifest.participants.length) {
      throw new Error(
        `count ${args.count} exceeds the manifest's ${args.manifest.participants.length} named participants`,
      );
    }
    return firstNParticipantNames(args.manifest, args.count);
  }
  const eligible = args.includeObservers
    ? args.manifest.participants
    : args.manifest.participants.filter((p) => p.costumeDir);
  if (args.count > eligible.length) {
    const label = args.includeObservers ? "participants" : "costumed participants";
    throw new Error(`count ${args.count} exceeds the manifest's ${eligible.length} ${label}`);
  }
  const seed = args.seed ?? Math.floor(Math.random() * 2 ** 31);
  const rng = seededRng(seed);
  const shuffled = shuffleSeeded(
    eligible.map((p) => p.name),
    rng,
  );
  return shuffled.slice(0, args.count);
}

/**
 * Render a display name from a `{participant}`-templated string, used by
 * the dashboard's multi-launch form so operators can label their fleet
 * uniformly (e.g. `"Bot {participant}"` → `"Bot alice"`, `"Bot bob"`).
 * Untemplated strings pass through as a constant prefix for everyone.
 * Empty / undefined falls back to a per-participant default.
 */
function templateDisplayName(
  template: string | undefined,
  participant: string,
): string | undefined {
  if (!template || template === "") return undefined;
  return template.replace(/\{participant\}/g, participant);
}

async function launchMultiRoute(
  opts: ControlServerOptions,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const surface = opts.surface;
  const mode = body.mode;
  if (mode !== "first-n" && mode !== "random") {
    throw new ControlServerError(400, '"mode" must be "first-n" or "random"');
  }
  const countRaw = body.count;
  if (typeof countRaw !== "number" || !Number.isFinite(countRaw) || countRaw <= 0) {
    throw new ControlServerError(400, '"count" must be a positive integer');
  }
  const count = Math.floor(countRaw);
  const maxUsers =
    typeof body.maxUsers === "number" && Number.isFinite(body.maxUsers) && body.maxUsers > 0
      ? Math.floor(body.maxUsers)
      : MULTI_LAUNCH_DEFAULT_MAX;
  if (count > maxUsers) {
    throw new ControlServerError(
      400,
      `count ${count} exceeds maxUsers ${maxUsers}; raise the cap to override`,
    );
  }

  const meetingURL = validateMeetingUrl(body.meetingURL);
  const ttlRaw = body.ttl;
  if (typeof ttlRaw !== "string") {
    throw new ControlServerError(400, '"ttl" must be a string');
  }
  let ttl: Ttl;
  try {
    ttl = parseDuration(ttlRaw);
  } catch (e) {
    throw new ControlServerError(400, (e as Error).message);
  }
  const network = body.network !== undefined ? validateNetworkField(body.network) : "none";
  const headless = typeof body.headless === "boolean" ? body.headless : false;
  const authBackend = body.authBackend ?? "jwt";
  if (authBackend !== "jwt" && authBackend !== "storage-state" && authBackend !== "none") {
    throw new ControlServerError(400, '"authBackend" must be "jwt", "storage-state", or "none"');
  }
  const storageStateFile =
    body.storageStateFile !== undefined && body.storageStateFile !== null
      ? String(body.storageStateFile)
      : undefined;
  const ssoStateFile =
    body.ssoStateFile !== undefined && body.ssoStateFile !== null
      ? String(body.ssoStateFile)
      : undefined;
  const displayNameTemplate =
    typeof body.displayNameTemplate === "string" ? body.displayNameTemplate : undefined;
  const runLocation = parseRunLocationField(body.runLocation);

  let seed: number | undefined;
  if (body.seed !== undefined) {
    if (typeof body.seed !== "number" || !Number.isFinite(body.seed)) {
      throw new ControlServerError(400, '"seed" must be a number when provided');
    }
    seed = Math.floor(body.seed);
  }
  const includeObservers =
    typeof body.includeObservers === "boolean" ? body.includeObservers : false;

  // Load the manifest. We reuse `opts.manifestPath` (the same path the
  // /assets/manifest endpoint caches against) so behavior matches the
  // dashboard's auto-match Select.
  if (!opts.manifestPath || !existsSync(opts.manifestPath)) {
    throw new ControlServerError(
      503,
      "multi-launch unavailable: no manifest configured on the control server",
    );
  }
  let manifest: Manifest;
  try {
    manifest = loadManifest(opts.manifestPath).manifest;
  } catch (e) {
    throw new ControlServerError(500, `failed to load manifest: ${(e as Error).message}`);
  }

  let picked: string[];
  try {
    picked = pickParticipantsForMultiLaunch({
      manifest,
      mode,
      count,
      seed,
      includeObservers,
    });
  } catch (e) {
    throw new ControlServerError(400, (e as Error).message);
  }

  const botIds: string[] = [];
  const errors: Array<{ participant: string; message: string }> = [];
  for (const participant of picked) {
    const spec: LaunchSpec = {
      meetingURL,
      participant,
      displayName: templateDisplayName(displayNameTemplate, participant),
      ttl,
      headless,
      network,
      authBackend,
      storageStateFile,
      ssoStateFile,
      runLocation,
    };
    try {
      const id = await surface.launchOne(spec);
      botIds.push(id);
    } catch (e) {
      errors.push({ participant, message: (e as Error).message });
    }
  }

  return {
    status: 202,
    body: {
      mode,
      count: picked.length,
      seed: seed ?? null,
      participants: picked,
      botIds,
      errors,
    },
  };
}

async function launchFromConfigRoute(
  opts: ControlServerOptions,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const surface = opts.surface;
  const configYaml = body.configYaml;
  if (typeof configYaml !== "string" || configYaml === "") {
    throw new ControlServerError(400, '"configYaml" must be a non-empty string');
  }
  let config: MeetingConfig;
  try {
    config = parseMeetingConfigText(configYaml);
  } catch (e) {
    throw new ControlServerError(400, `meeting config parse failed: ${(e as Error).message}`);
  }

  const headless = typeof body.headless === "boolean" ? body.headless : false;
  const overrideAuth = body.authBackend;
  if (
    overrideAuth !== undefined &&
    overrideAuth !== "jwt" &&
    overrideAuth !== "storage-state" &&
    overrideAuth !== "none"
  ) {
    throw new ControlServerError(
      400,
      '"authBackend" must be "jwt", "storage-state", or "none" when provided',
    );
  }
  const overrideStorageStateFile =
    typeof body.storageStateFile === "string" ? body.storageStateFile : undefined;
  const overrideSsoStateFile =
    typeof body.ssoStateFile === "string" ? body.ssoStateFile : undefined;

  // Default TTL: per-bot ttl wins, then meeting-level, then 5m.
  const defaultTtl = config.ttl ?? "5m";
  // Default network: per-bot network wins, then meeting-level, then "none".
  const defaultNetwork = config.network ?? "none";
  // Default auth backend: per-bot auth wins, then meeting-level, then
  // the override on the request body, then "jwt".
  const defaultAuth =
    (config.auth as "jwt" | "storage-state" | "none" | undefined) ??
    (overrideAuth as "jwt" | "storage-state" | "none" | undefined) ??
    "jwt";

  const botIds: string[] = [];
  const errors: Array<{ index: number; participant?: string; message: string }> = [];
  for (let i = 0; i < config.bots.length; i++) {
    const bot = config.bots[i];
    const ttlRaw = bot.ttl ?? defaultTtl;
    let ttl: Ttl;
    try {
      ttl = parseDuration(ttlRaw);
    } catch (e) {
      errors.push({
        index: i,
        participant: bot.participant,
        message: `invalid ttl "${ttlRaw}": ${(e as Error).message}`,
      });
      continue;
    }
    const network = bot.network ?? defaultNetwork;
    const authBackend = (bot.auth ?? defaultAuth) as "jwt" | "storage-state" | "none";
    const spec: LaunchSpec = {
      meetingURL: config.meetingUrl,
      participant: bot.participant,
      ttl,
      headless,
      network,
      authBackend,
      storageStateFile: overrideStorageStateFile,
      ssoStateFile: overrideSsoStateFile,
    };
    try {
      const id = await surface.launchOne(spec);
      botIds.push(id);
    } catch (e) {
      errors.push({
        index: i,
        participant: bot.participant,
        message: (e as Error).message,
      });
    }
  }

  return {
    status: 202,
    body: {
      meetingUrl: config.meetingUrl,
      count: botIds.length,
      botIds,
      errors,
    },
  };
}

function previewFromConfigRoute(body: Record<string, unknown>): RouteResult {
  const configYaml = body.configYaml;
  if (typeof configYaml !== "string" || configYaml === "") {
    throw new ControlServerError(400, '"configYaml" must be a non-empty string');
  }
  let config: MeetingConfig;
  try {
    config = parseMeetingConfigText(configYaml);
  } catch (e) {
    throw new ControlServerError(400, `meeting config parse failed: ${(e as Error).message}`);
  }
  return {
    status: 200,
    body: {
      meetingUrl: config.meetingUrl,
      ttl: config.ttl ?? null,
      network: config.network ?? null,
      auth: config.auth ?? null,
      botCount: config.bots.length,
      bots: config.bots,
      meta: config.meta ?? null,
    },
  };
}

function validateMeetingUrl(raw: unknown): string {
  if (typeof raw !== "string" || raw === "") {
    throw new ControlServerError(400, '"meetingURL" must be a non-empty string');
  }
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw new ControlServerError(400, '"meetingURL" is not a valid URL');
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new ControlServerError(400, '"meetingURL" must use http or https');
  }
  return raw;
}

function validateNetworkField(raw: unknown): string {
  if (typeof raw !== "string") {
    throw new ControlServerError(400, '"network" must be a string');
  }
  if (!NETSIM_PRESETS.includes(raw)) {
    throw new ControlServerError(
      400,
      `"network" must be one of: ${NETSIM_PRESETS.join(", ")} (got "${raw}")`,
    );
  }
  return raw;
}

// ──────────────────────────────────────────────────────────────────────
// OAuth session capture endpoints. Sibling of the HCL SSO recapture
// flow but for Google OAuth sessions used by storage-state auth. Files
// land at <runDir>/auth/<label>.json; the legacy hcl-sso.json is
// excluded from this listing because it is owned by the SSO flow.
// ──────────────────────────────────────────────────────────────────────

interface OauthSessionInfo {
  label: string;
  filePath: string;
  capturedAt: number;
  ageHours: number;
  size: number;
}

function oauthSessionsRoute(opts: ControlServerOptions): RouteResult {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "oauth sessions unavailable: control server was started without a runDir",
    );
  }
  const authDir = join(opts.runDir, "auth");
  let entries: string[];
  try {
    entries = readdirSync(authDir);
  } catch {
    return { status: 200, body: { sessions: [] } };
  }
  const now = Date.now();
  const sessions: OauthSessionInfo[] = [];
  for (const name of entries) {
    if (!name.endsWith(".json")) continue;
    if (name === DEFAULT_SSO_STATE_BASENAME) continue;
    if (name.startsWith(".") || name.startsWith("_")) continue;
    const label = name.slice(0, -".json".length);
    if (!OAUTH_LABEL_PATTERN.test(label)) continue;
    const filePath = join(authDir, name);
    try {
      const st = statSync(filePath);
      if (!st.isFile()) continue;
      sessions.push({
        label,
        filePath,
        capturedAt: st.mtimeMs,
        ageHours: (now - st.mtimeMs) / (1000 * 60 * 60),
        size: st.size,
      });
    } catch {
      continue;
    }
  }
  sessions.sort((a, b) => a.label.localeCompare(b.label));
  return { status: 200, body: { sessions } };
}

async function oauthCaptureStartRoute(
  opts: ControlServerOptions,
  ssoState: SsoState,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "oauth capture unavailable: control server was started without a runDir",
    );
  }
  const label = body.label;
  if (typeof label !== "string" || label === "") {
    throw new ControlServerError(400, '"label" must be a non-empty string');
  }
  if (!OAUTH_LABEL_PATTERN.test(label)) {
    throw new ControlServerError(
      400,
      `"label" must match ${OAUTH_LABEL_PATTERN.source} (got "${label}")`,
    );
  }
  let startUrl = "https://app.videocall.rs/";
  if (body.startUrl !== undefined) {
    if (typeof body.startUrl !== "string") {
      throw new ControlServerError(400, '"startUrl" must be a string when provided');
    }
    try {
      const u = new URL(body.startUrl);
      if (u.protocol !== "http:" && u.protocol !== "https:") {
        throw new Error("only http(s) URLs are accepted");
      }
    } catch (e) {
      throw new ControlServerError(400, `invalid startUrl: ${(e as Error).message}`);
    }
    startUrl = body.startUrl;
  }
  const factory = opts.ssoCaptureFactory ?? openSsoCaptureBrowser;
  const sessionId = randomUUID();
  let session: SsoCaptureSession;
  try {
    session = await factory({ startUrl });
  } catch (e) {
    throw new ControlServerError(
      500,
      `oauth capture: browser launch failed: ${(e as Error).message}`,
    );
  }
  const startedAt = Date.now();
  const idleTimer = setTimeout(() => {
    const entry = ssoState.oauthCaptureSessions.get(sessionId);
    if (entry === undefined) return;
    ssoState.oauthCaptureSessions.delete(sessionId);
    void entry.session.close().catch((err: unknown) => {
      console.warn(
        `[control] idle-timeout teardown of oauth capture ${sessionId} failed: ${(err as Error).message}`,
      );
    });
    console.log(
      `[control] oauth capture ${sessionId} auto-cancelled after idle timeout (${ssoState.idleTimeout}ms)`,
    );
  }, ssoState.idleTimeout);
  if (typeof idleTimer.unref === "function") idleTimer.unref();
  ssoState.oauthCaptureSessions.set(sessionId, {
    id: sessionId,
    label,
    startUrl,
    startedAt,
    session,
    idleTimer,
  });
  return {
    status: 201,
    body: { captureSessionId: sessionId, label, startUrl, startedAt },
  };
}

async function oauthCaptureCompleteRoute(
  opts: ControlServerOptions,
  ssoState: SsoState,
  sessionId: string,
): Promise<RouteResult> {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "oauth capture unavailable: control server was started without a runDir",
    );
  }
  const entry = ssoState.oauthCaptureSessions.get(sessionId);
  if (entry === undefined) {
    throw new ControlServerError(404, `oauth capture session ${sessionId} not found`);
  }
  ssoState.oauthCaptureSessions.delete(sessionId);
  clearTimeout(entry.idleTimer);
  const outPath = storageStatePath(opts.runDir, entry.label);
  try {
    await entry.session.saveAndClose(outPath);
  } catch (e) {
    await entry.session.close().catch(() => {});
    throw new ControlServerError(500, `oauth capture save failed: ${(e as Error).message}`);
  }
  // Return the freshly-captured session's metadata so the dashboard can
  // refresh its list without a second round-trip.
  let info: OauthSessionInfo;
  try {
    const st = statSync(outPath);
    info = {
      label: entry.label,
      filePath: outPath,
      capturedAt: st.mtimeMs,
      ageHours: (Date.now() - st.mtimeMs) / (1000 * 60 * 60),
      size: st.size,
    };
  } catch (e) {
    throw new ControlServerError(500, `oauth capture stat failed: ${(e as Error).message}`);
  }
  return { status: 200, body: info };
}

function oauthCaptureCancelRoute(ssoState: SsoState, sessionId: string): Promise<RouteResult> {
  const entry = ssoState.oauthCaptureSessions.get(sessionId);
  if (entry === undefined) {
    throw new ControlServerError(404, `oauth capture session ${sessionId} not found`);
  }
  ssoState.oauthCaptureSessions.delete(sessionId);
  clearTimeout(entry.idleTimer);
  return entry.session
    .close()
    .then(() => ({ status: 200, body: { captureSessionId: sessionId, cancelled: true } }));
}

function oauthSessionDeleteRoute(opts: ControlServerOptions, label: string): RouteResult {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "oauth sessions unavailable: control server was started without a runDir",
    );
  }
  if (!OAUTH_LABEL_PATTERN.test(label)) {
    throw new ControlServerError(
      400,
      `"label" must match ${OAUTH_LABEL_PATTERN.source} (got "${label}")`,
    );
  }
  // Refuse to touch the HCL SSO state file via this endpoint — it is
  // owned by the separate SSO recapture flow. Operators who want to
  // wipe HCL SSO state delete the file out-of-band.
  if (`${label}.json` === DEFAULT_SSO_STATE_BASENAME) {
    throw new ControlServerError(
      400,
      `"${label}" refers to the HCL SSO state file; use the SSO panel to manage it`,
    );
  }
  const filePath = storageStatePath(opts.runDir, label);
  try {
    unlinkSync(filePath);
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    if (code === "ENOENT") {
      throw new ControlServerError(404, `oauth session "${label}" not found`);
    }
    throw new ControlServerError(500, `delete failed: ${(e as Error).message}`);
  }
  return { status: 200, body: { label, deleted: true } };
}

// ──────────────────────────────────────────────────────────────────────
// Prep-assets background-job routes. The work itself lives in
// `prep-assets.ts`; here we just register the job, hand back the id,
// and stream log lines as SSE on the dedicated /stream endpoint.
// ──────────────────────────────────────────────────────────────────────

const DEFAULT_PREP_MANIFEST_RELATIVE = "bot/conversation/manifest.yaml";
const DEFAULT_PREP_COSTUME_SOURCE_RELATIVE = "bot/assets/costumes";

async function prepAssetsStartRoute(
  opts: ControlServerOptions,
  jobs: Map<string, PrepAssetsJob>,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  if (!opts.runDir) {
    throw new ControlServerError(
      503,
      "prep-assets unavailable: control server was started without a runDir",
    );
  }
  // Only one prep job at a time per daemon — ffmpeg is expensive and
  // running two concurrently against the same output dir would race
  // on the same files. The 409 carries the running jobId so the
  // dashboard can re-attach to its stream instead of spawning a dup.
  for (const job of jobs.values()) {
    if (job.status === "running") {
      throw new ControlServerError(409, `prep-assets job ${job.jobId} is already running`);
    }
  }

  // Validate optional override fields. The dashboard form sends fixed
  // defaults today, but the API is reachable from curl and must be
  // hardened against path-traversal exactly like the other file-bearing
  // endpoints. `validatePrepAssetsPath` throws plain Error on bad
  // input — wrap it in a 400-shaped ControlServerError so the response
  // matches the route's contract.
  const safeValidate = (raw: unknown, field: string): string => {
    try {
      return validatePrepAssetsPath(raw, field);
    } catch (e) {
      throw new ControlServerError(400, (e as Error).message);
    }
  };
  const manifestPath =
    body.manifestPath !== undefined
      ? safeValidate(body.manifestPath, "manifestPath")
      : (opts.manifestPath ?? defaultPrepManifest(opts.runDir));
  const costumeSource =
    body.costumeSource !== undefined
      ? safeValidate(body.costumeSource, "costumeSource")
      : defaultPrepCostumeSource(opts.runDir);
  const outputDir =
    body.outputDir !== undefined ? safeValidate(body.outputDir, "outputDir") : opts.runDir;
  let participants: string[] | undefined;
  if (body.participants !== undefined) {
    if (!Array.isArray(body.participants)) {
      throw new ControlServerError(400, '"participants" must be an array of strings');
    }
    participants = body.participants.map((p: unknown, idx: number) => {
      if (typeof p !== "string" || p === "") {
        throw new ControlServerError(400, `participants[${idx}] must be a non-empty string`);
      }
      if (!/^[A-Za-z0-9._@+-]+$/.test(p)) {
        throw new ControlServerError(400, `participants[${idx}] contains invalid characters`);
      }
      return p;
    });
  }

  const job = createPrepAssetsJob();
  jobs.set(job.jobId, job);
  const opts2: PrepAssetsOptions = {
    manifestPath,
    costumeSource,
    outputDir,
    participants,
  };
  // Fire-and-forget. Kicked off via a microtask so the response below
  // reports the initial "running" state even when the worker can fail
  // synchronously (e.g. missing manifest) — the dashboard then sees
  // the transition on its next status poll, matching the contract.
  queueMicrotask(() => {
    void runPrepAssetsJob(job, opts2).catch((e: unknown) => {
      job.status = "failed";
      job.error = (e as Error).message;
      job.finishedAt = Date.now();
    });
  });
  return {
    status: 202,
    body: {
      jobId: job.jobId,
      status: "running",
      startedAt: job.startedAt,
    },
  };
}

function prepAssetsStatusRoute(jobs: Map<string, PrepAssetsJob>, jobId: string): RouteResult {
  const job = jobs.get(jobId);
  if (job === undefined) {
    throw new ControlServerError(404, `prep-assets job ${jobId} not found`);
  }
  return {
    status: 200,
    body: snapshotPrepAssetsJob(job),
  };
}

function prepAssetsForgetRoute(jobs: Map<string, PrepAssetsJob>, jobId: string): RouteResult {
  const job = jobs.get(jobId);
  if (job === undefined) {
    throw new ControlServerError(404, `prep-assets job ${jobId} not found`);
  }
  // We do NOT actually terminate the underlying work — ffmpeg is
  // expensive to restart, and the dashboard's "Cancel" button is
  // explicitly documented as "close the modal but let it finish in
  // the background". Just drop the record so the dashboard's polling
  // exits gracefully.
  if (job.status === "running") {
    throw new ControlServerError(
      409,
      `prep-assets job ${jobId} is still running; close the dashboard modal but let the job finish`,
    );
  }
  jobs.delete(jobId);
  return { status: 200, body: { jobId, deleted: true } };
}

function snapshotPrepAssetsJob(job: PrepAssetsJob): Record<string, unknown> {
  return {
    jobId: job.jobId,
    status: job.status,
    startedAt: job.startedAt,
    finishedAt: job.finishedAt ?? null,
    stdoutLog: job.stdoutLog,
    exitCode: job.exitCode,
    error: job.error ?? null,
    audioPrepped: job.audioPrepped,
    costumesPrepped: job.costumesPrepped,
  };
}

function handlePrepAssetsStream(
  req: IncomingMessage,
  res: ServerResponse,
  jobs: Map<string, PrepAssetsJob>,
  jobId: string,
): void {
  // Auth was already enforced by `handleRequest` before dispatch — the
  // route-aware authenticate runs there, so by the time this function
  // is called the request is authorized.
  const job = jobs.get(jobId);
  if (job === undefined) {
    sendJson(res, 404, { error: `prep-assets job ${jobId} not found` });
    return;
  }
  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
    // Tell intermediaries (nginx, etc.) not to buffer.
    "x-accel-buffering": "no",
  });

  // Send the existing log buffer first so a late subscriber sees the
  // whole history.
  for (const line of job.stdoutLog) {
    res.write(`data: ${line}\n\n`);
  }
  // If the job already finished, close immediately.
  if (job.status !== "running") {
    res.write(
      `event: end\ndata: ${JSON.stringify({ status: job.status, exitCode: job.exitCode })}\n\n`,
    );
    res.end();
    return;
  }

  const onLine = (line: string | null): void => {
    if (line === null) {
      res.write(
        `event: end\ndata: ${JSON.stringify({ status: job.status, exitCode: job.exitCode })}\n\n`,
      );
      res.end();
      return;
    }
    res.write(`data: ${line}\n\n`);
  };
  job.subscribers.add(onLine);
  req.on("close", () => {
    job.subscribers.delete(onLine);
  });
}

function defaultPrepManifest(runDir: string): string {
  // Convention used by the CLI defaults: prep-assets is run from the
  // repo root so the manifest lives at <repo>/bot/conversation/manifest.yaml.
  // We re-derive it by walking up from runDir (`<repo>/e2e/bots-app/run`)
  // — defaultPrepManifest("/repo/e2e/bots-app/run") → "/repo/bot/.../manifest.yaml".
  // Falls back to a sentinel that the runner's existsSync check will
  // reject if the layout differs.
  return join(runDir, "..", "..", "..", DEFAULT_PREP_MANIFEST_RELATIVE);
}

function defaultPrepCostumeSource(runDir: string): string {
  return join(runDir, "..", "..", "..", DEFAULT_PREP_COSTUME_SOURCE_RELATIVE);
}

// ──────────────────────────────────────────────────────────────────────
// SSH host registry routes. All four CRUD verbs + a synchronous
// probe. Persistence lives in `<runDir>/hosts.json`; see ssh-hosts.ts.
// ──────────────────────────────────────────────────────────────────────

async function listHostsRoute(opts: ControlServerOptions): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const hosts = await listHosts(runDir);
  return { status: 200, body: { hosts } };
}

async function addHostRoute(
  opts: ControlServerOptions,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const spec = parseHostInput(body);
  const host = await addHost(runDir, spec);
  return { status: 201, body: { host } };
}

async function updateHostRoute(
  opts: ControlServerOptions,
  label: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const patch = parseHostPatch(body);
  const host = await updateHost(runDir, label, patch);
  return { status: 200, body: { host } };
}

async function removeHostRoute(opts: ControlServerOptions, label: string): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  await removeHost(runDir, label);
  return { status: 204, body: null };
}

async function testHostRoute(opts: ControlServerOptions, label: string): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const result = await testHost(runDir, label);
  // Always 200 — the result body carries `ok: false` for the failure
  // case so the dashboard can render a colored chip without falling
  // through to the error-banner path.
  return { status: 200, body: result };
}

/**
 * `POST /hosts/:label/preview-launch` — pure command-construction
 * endpoint that returns the exact `ssh` argv (and human-readable
 * rendering) the dashboard would invoke if the operator clicked
 * "Launch Bot" with the supplied launch spec + this host.
 *
 * This endpoint MUST NOT spawn anything. It exists solely to back the
 * "SSH command preview" surface in the Launch + Multi-launch forms.
 * Validation reuses the same field-by-field checks `launchOne` applies
 * to the real launch endpoint so an invalid spec surfaces the same
 * error message in the preview as it would on a real submit.
 *
 * Returns 404 if `label` is not registered; 400 on invalid launch spec.
 */
async function previewLaunchRoute(
  opts: ControlServerOptions,
  label: string,
  body: Record<string, unknown>,
): Promise<RouteResult> {
  const runDir = requireRunDir(opts);
  const host = await getHost(runDir, label);
  if (host === null) {
    throw new ControlServerError(404, `ssh host "${label}" not found`);
  }
  // Reuse the existing field-validation logic from the real launch
  // path so a typo'd `participant` or unknown `network` surfaces in
  // the preview just like it would on a real submit. We synthesize a
  // run-location of { kind: "ssh", hostLabel } from the URL so the
  // operator does not have to repeat it in the body.
  const spec = parseLaunchSpecForPreview(body);
  // Mirror the launcher's SSO-wrap gate so the preview shows EXACTLY
  // the command the launcher would run: host.forwardSsoState ON,
  // authBackend === "jwt", and a local SSO state file exists. The
  // dashboard's "what will execute" promise depends on this match.
  const ssoWrap =
    host.forwardSsoState !== false &&
    spec.authBackend === "jwt" &&
    existsSync(defaultSsoStatePath(runDir));
  const render = buildSshCommand(
    host,
    {
      host,
      ttl: formatDuration(spec.ttl),
      meetingURL: spec.meetingURL,
      participant: spec.participant,
      network: spec.network === "none" ? null : spec.network,
      authBackend: spec.authBackend,
      displayName: spec.displayName ?? null,
      headless: spec.headless,
    },
    { ssoWrap },
  );
  return {
    status: 200,
    body: {
      argv: render.argv,
      display: render.display,
      remoteCommand: render.remoteCommand,
    },
  };
}

/**
 * `POST /hosts/preview` — preview the SSH command for an UNSAVED host
 * (the Add Host dialog calls this as the operator types so the
 * displayed command always reflects the form's current state). The
 * host config arrives in the body and is validated exactly as it
 * would be on `POST /hosts` — invalid values surface a 400 in the
 * preview just like they would on save.
 *
 * Returns the same `{ argv, display, remoteCommand }` shape as
 * `/hosts/:label/preview-launch`. Nothing is persisted.
 *
 * The optional `launchSpec` field overrides the default placeholder
 * tokens (`<participant>`, `<meeting-url>`, etc.); when omitted the
 * preview uses visibly-distinct placeholder strings so the operator
 * sees what gets filled in at launch time.
 */
function previewHostRoute(opts: ControlServerOptions, body: Record<string, unknown>): RouteResult {
  const hostBody = body.host;
  if (hostBody === null || typeof hostBody !== "object" || Array.isArray(hostBody)) {
    throw new ControlServerError(400, '"host" must be an object');
  }
  const spec = parseHostInput(hostBody as Record<string, unknown>);
  // Run validation by building the host (same path as `addHost`), but
  // without persisting. `buildHost` throws SshHostValidationError on
  // bad input; the outer error handler translates that to 400.
  let preview: ReturnType<typeof buildHostForPreview>;
  try {
    preview = buildHostForPreview(spec);
  } catch (e) {
    if (e instanceof SshHostValidationError) {
      throw new ControlServerError(400, e.message);
    }
    throw e;
  }
  // Optional launch overrides. The default placeholder tokens make it
  // obvious to the operator that those values get filled in at launch
  // time (`<participant>` is not a real participant name).
  const launchOverride =
    typeof body.launchSpec === "object" && body.launchSpec !== null
      ? (body.launchSpec as Record<string, unknown>)
      : null;
  const ttl =
    launchOverride !== null && typeof launchOverride.ttl === "string" ? launchOverride.ttl : "5m";
  const meetingURL =
    launchOverride !== null && typeof launchOverride.meetingURL === "string"
      ? launchOverride.meetingURL
      : "<meeting-url>";
  const participant =
    launchOverride !== null && typeof launchOverride.participant === "string"
      ? launchOverride.participant
      : "<participant>";
  const authBackend =
    launchOverride !== null && typeof launchOverride.authBackend === "string"
      ? launchOverride.authBackend
      : "<auth>";
  const network =
    launchOverride !== null &&
    typeof launchOverride.network === "string" &&
    launchOverride.network !== "none"
      ? launchOverride.network
      : null;
  const displayName =
    launchOverride !== null && typeof launchOverride.displayName === "string"
      ? launchOverride.displayName
      : null;
  // Apply the same SSO-wrap decision the real launcher would: only
  // when the unsaved host has `forwardSsoState !== false`, the
  // override sets `authBackend === "jwt"`, AND a local SSO state file
  // exists at the conventional path. The preview endpoint runs
  // BEFORE the host is saved, so we can't probe a label-scoped state
  // file — the global `<runDir>/auth/hcl-sso.json` is the canonical
  // capture target.
  const ssoWrap =
    preview.forwardSsoState !== false &&
    authBackend === "jwt" &&
    opts.runDir !== undefined &&
    opts.runDir !== null &&
    existsSync(defaultSsoStatePath(opts.runDir));
  const render = buildSshCommand(
    preview,
    {
      host: preview,
      ttl,
      meetingURL,
      participant,
      network,
      authBackend,
      displayName,
      headless: true,
    },
    { ssoWrap },
  );
  return {
    status: 200,
    body: {
      argv: render.argv,
      display: render.display,
      remoteCommand: render.remoteCommand,
    },
  };
}

/**
 * Validate the subset of {@link LaunchSpec} fields the preview endpoint
 * needs. This mirrors the launch-form validators in `launchOne` but
 * skips fields that do not affect the SSH command shape (costume /
 * audio / storage-state path / SSO state path are irrelevant to the
 * preview because they are not part of the remote bash command).
 */
function parseLaunchSpecForPreview(body: Record<string, unknown>): {
  meetingURL: string;
  participant: string;
  displayName: string | undefined;
  ttl: Ttl;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
} {
  const meetingURL = body.meetingURL;
  if (typeof meetingURL !== "string" || meetingURL === "") {
    throw new ControlServerError(400, '"meetingURL" must be a non-empty string');
  }
  try {
    const url = new URL(meetingURL);
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new ControlServerError(400, `"meetingURL" must use http or https`);
    }
  } catch {
    throw new ControlServerError(400, `"meetingURL" is not a valid URL`);
  }
  const participant = body.participant;
  if (typeof participant !== "string" || participant === "") {
    throw new ControlServerError(400, '"participant" must be a non-empty string');
  }
  if (!/^[A-Za-z0-9._@+-]+$/.test(participant)) {
    throw new ControlServerError(400, '"participant" contains invalid characters');
  }
  const displayName = body.displayName;
  if (displayName !== undefined && typeof displayName !== "string") {
    throw new ControlServerError(400, '"displayName" must be a string when provided');
  }
  const ttlRaw = body.ttl;
  if (typeof ttlRaw !== "string") {
    throw new ControlServerError(400, '"ttl" must be a string');
  }
  let ttl: Ttl;
  try {
    ttl = parseDuration(ttlRaw);
  } catch (e) {
    throw new ControlServerError(400, (e as Error).message);
  }
  const headless = body.headless;
  if (typeof headless !== "boolean") {
    throw new ControlServerError(400, '"headless" must be a boolean');
  }
  const network = body.network;
  if (typeof network !== "string") {
    throw new ControlServerError(400, '"network" must be a string');
  }
  if (!NETSIM_PRESETS.includes(network)) {
    throw new ControlServerError(
      400,
      `"network" must be one of: ${NETSIM_PRESETS.join(", ")} (got "${network}")`,
    );
  }
  const authBackend = body.authBackend;
  if (authBackend !== "jwt" && authBackend !== "storage-state" && authBackend !== "none") {
    throw new ControlServerError(400, '"authBackend" must be "jwt", "storage-state", or "none"');
  }
  return {
    meetingURL,
    participant,
    displayName: typeof displayName === "string" && displayName !== "" ? displayName : undefined,
    ttl,
    headless,
    network,
    authBackend,
  };
}

function parseHostInput(body: Record<string, unknown>): SshHostInput {
  const label = body.label;
  if (typeof label !== "string") throw new ControlServerError(400, '"label" must be a string');
  const host = body.host;
  if (typeof host !== "string") throw new ControlServerError(400, '"host" must be a string');
  const reposPath = body.reposPath;
  if (typeof reposPath !== "string") {
    throw new ControlServerError(400, '"reposPath" must be a string');
  }
  const user = body.user;
  if (user !== undefined && typeof user !== "string") {
    throw new ControlServerError(400, '"user" must be a string when provided');
  }
  const sshKey = body.sshKey;
  if (sshKey !== undefined && sshKey !== null && typeof sshKey !== "string") {
    throw new ControlServerError(400, '"sshKey" must be a string or null');
  }
  const notes = body.notes;
  if (notes !== undefined && notes !== null && typeof notes !== "string") {
    throw new ControlServerError(400, '"notes" must be a string or null');
  }
  const shell = body.shell;
  if (shell !== undefined && shell !== null && typeof shell !== "string") {
    throw new ControlServerError(400, '"shell" must be a string or null');
  }
  const profileFile = body.profileFile;
  if (profileFile !== undefined && profileFile !== null && typeof profileFile !== "string") {
    throw new ControlServerError(400, '"profileFile" must be a string or null');
  }
  const preCommand = body.preCommand;
  if (preCommand !== undefined && preCommand !== null && typeof preCommand !== "string") {
    throw new ControlServerError(400, '"preCommand" must be a string or null');
  }
  const forwardSsoState = body.forwardSsoState;
  if (forwardSsoState !== undefined && typeof forwardSsoState !== "boolean") {
    throw new ControlServerError(400, '"forwardSsoState" must be a boolean when provided');
  }
  return {
    label,
    host,
    reposPath,
    user: user as string | undefined,
    sshKey: sshKey === undefined ? undefined : (sshKey as string | null),
    notes: notes === undefined ? undefined : (notes as string | null),
    shell: shell === undefined ? undefined : (shell as string | null),
    profileFile: profileFile === undefined ? undefined : (profileFile as string | null),
    preCommand: preCommand === undefined ? undefined : (preCommand as string | null),
    forwardSsoState: forwardSsoState as boolean | undefined,
  };
}

function parseHostPatch(body: Record<string, unknown>): SshHostPatch {
  const patch: SshHostPatch = {};
  if (body.host !== undefined) {
    if (typeof body.host !== "string") {
      throw new ControlServerError(400, '"host" must be a string when provided');
    }
    patch.host = body.host;
  }
  if (body.user !== undefined) {
    if (typeof body.user !== "string") {
      throw new ControlServerError(400, '"user" must be a string when provided');
    }
    patch.user = body.user;
  }
  if (body.sshKey !== undefined) {
    if (body.sshKey !== null && typeof body.sshKey !== "string") {
      throw new ControlServerError(400, '"sshKey" must be a string or null when provided');
    }
    patch.sshKey = body.sshKey as string | null;
  }
  if (body.reposPath !== undefined) {
    if (typeof body.reposPath !== "string") {
      throw new ControlServerError(400, '"reposPath" must be a string when provided');
    }
    patch.reposPath = body.reposPath;
  }
  if (body.notes !== undefined) {
    if (body.notes !== null && typeof body.notes !== "string") {
      throw new ControlServerError(400, '"notes" must be a string or null when provided');
    }
    patch.notes = body.notes as string | null;
  }
  if (body.shell !== undefined) {
    if (body.shell !== null && typeof body.shell !== "string") {
      throw new ControlServerError(400, '"shell" must be a string or null when provided');
    }
    patch.shell = body.shell as string | null;
  }
  if (body.profileFile !== undefined) {
    if (body.profileFile !== null && typeof body.profileFile !== "string") {
      throw new ControlServerError(400, '"profileFile" must be a string or null when provided');
    }
    patch.profileFile = body.profileFile as string | null;
  }
  if (body.preCommand !== undefined) {
    if (body.preCommand !== null && typeof body.preCommand !== "string") {
      throw new ControlServerError(400, '"preCommand" must be a string or null when provided');
    }
    patch.preCommand = body.preCommand as string | null;
  }
  if (body.forwardSsoState !== undefined) {
    if (typeof body.forwardSsoState !== "boolean") {
      throw new ControlServerError(400, '"forwardSsoState" must be a boolean when provided');
    }
    patch.forwardSsoState = body.forwardSsoState;
  }
  return patch;
}

/**
 * `GET /bots/:id/log?since=<n>` — paginates the rolling log buffer
 * stored on the registry entry. For SSH-hosted bots this is the SSH
 * ChildProcess's stdout/stderr; for local bots it's currently always
 * empty (Playwright bots log to stdout directly, not to the registry).
 * The wire shape is stable across both kinds so the dashboard can use
 * a single fetch path.
 */
function botLogRoute(surface: OrchestratorControlSurface, botId: string, url: URL): RouteResult {
  const entry = surface.getRegistry().get(botId);
  if (entry === undefined) {
    throw new ControlServerError(404, `bot ${botId} not found`);
  }
  let since = 0;
  const sinceRaw = url.searchParams.get("since");
  if (sinceRaw !== null) {
    const n = Number.parseInt(sinceRaw, 10);
    if (!Number.isFinite(n) || n < 0) {
      throw new ControlServerError(400, '"since" must be a non-negative integer');
    }
    since = n;
  }
  if (entry.sshHandle !== null) {
    return { status: 200, body: readLogWindow(entry.sshHandle, since) };
  }
  // Local bot — read the orchestrator-side rolling buffer. Currently
  // populated by the auto-prime helper (priming progress events) and
  // empty otherwise. Returns the same wire shape as the SSH path so
  // the dashboard's polling loop is transport-agnostic.
  return { status: 200, body: readLocalLogWindow(entry, since) };
}

function countLiveBots(registry: Map<string, BotRegistryEntry>): number {
  let n = 0;
  for (const entry of registry.values()) {
    if (entry.status !== "done" && entry.status !== "failed") n++;
  }
  return n;
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

/**
 * Read up to 1 MiB of JSON body. Anything larger is rejected to
 * keep an accidentally-attached `cat large-file | curl --data @-` from
 * pinning the orchestrator's event loop. The control API only ever
 * accepts tiny mutation payloads, so the cap is generous by ~3 orders
 * of magnitude.
 */
async function readJsonBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const MAX_BYTES = 1024 * 1024;
  const chunks: Buffer[] = [];
  let total = 0;
  return new Promise<Record<string, unknown>>((resolve, reject) => {
    req.on("data", (chunk: Buffer) => {
      total += chunk.length;
      if (total > MAX_BYTES) {
        reject(new ControlServerError(413, "request body exceeds 1 MiB limit"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      if (chunks.length === 0) {
        resolve({});
        return;
      }
      const raw = Buffer.concat(chunks).toString("utf8");
      try {
        const parsed = JSON.parse(raw);
        if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
          reject(new ControlServerError(400, "request body must be a JSON object"));
          return;
        }
        resolve(parsed as Record<string, unknown>);
      } catch (e) {
        reject(new ControlServerError(400, `invalid JSON body: ${(e as Error).message}`));
      }
    });
    req.on("error", (e) => reject(e));
  });
}
