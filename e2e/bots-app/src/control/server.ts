import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { statSync } from "node:fs";

import { defaultSsoStatePath } from "../auth/storage-state";
import {
  DEFAULT_SSO_START_URL,
  openSsoCaptureBrowser,
  type SsoCaptureSession,
} from "../auth/sso-capture";
import { NETSIM_PRESETS } from "../meeting-config";
import { formatDuration, parseDuration, type Ttl } from "../ttl";
import { extractBearerToken, tokensMatch } from "./auth";
import {
  deleteProfile,
  listProfiles,
  ProfileExistsError,
  ProfileNotFoundError,
  ProfileValidationError,
  readProfile,
  saveProfile,
  type ProfileBotSpec,
} from "./profiles";
import {
  type BotRegistryEntry,
  type BotSnapshot,
  snapshotEntry,
  sweepStaleEntries,
} from "./registry";

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
}

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
  const idleTimeout = opts.ssoRecaptureIdleTimeoutMs ?? SSO_RECAPTURE_IDLE_TIMEOUT_MS;

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
  };

  return new Promise<ControlServerHandle>((resolve, reject) => {
    const server = createServer((req, res) => {
      handleRequest(req, res, opts, { ssoRecaptureSessions, idleTimeout }).catch((err: unknown) => {
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
  idleTimeout: number;
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
  opts: ControlServerOptions,
  ssoState: SsoState,
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

  try {
    const result = await route(req, opts, pathname, method, ssoState);
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
): Promise<RouteResult> {
  const surface = opts.surface;
  if (method === "GET" && pathname === "/bots") {
    return listBots(surface);
  }
  if (method === "POST" && pathname === "/launch") {
    const body = await readJsonBody(req);
    return launchOne(surface, body);
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
    }
  }

  const botPath = /^\/bots\/([^/]+)(?:\/([^/]+))?$/.exec(pathname);
  if (botPath) {
    const botId = decodeURIComponent(botPath[1]);
    const sub = botPath[2];
    if (sub === undefined) {
      if (method === "GET") return getOneBot(surface, botId);
      if (method === "DELETE") return killBot(surface, botId);
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
  return {
    meetingURL: o.meetingURL,
    participant: o.participant,
    displayName,
    ttl: o.ttl,
    headless: o.headless,
    network: o.network,
    authBackend: auth,
    storageStateFile,
  };
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
  // `runLocation` is dashboard-only metadata; we accept it but only
  // honor "local" today. Anything else is rejected so a future
  // backend implementation can't be silently downgraded.
  const runLocation = body.runLocation;
  if (runLocation !== undefined && runLocation !== "local") {
    throw new ControlServerError(
      400,
      'only "local" runLocation is wired today; see discussion #793',
    );
  }
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
  };
  const newId = await surface.launchOne(spec);
  return { status: 201, body: { botId: newId } };
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
