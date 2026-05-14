import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

import { NETSIM_PRESETS } from "../meeting-config";
import { formatDuration, parseDuration, type Ttl } from "../ttl";
import { extractBearerToken, tokensMatch } from "./auth";
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
  authBackend: "jwt" | "storage-state";
  storageStateFile?: string;
}

/**
 * Bind options for the control server. `port: 0` (the default) asks
 * the kernel for a free ephemeral port — used by `--ctl-port auto`
 * on the CLI and by the in-process integration tests.
 */
export interface ControlServerOptions {
  port: number;
  token: string;
  surface: OrchestratorControlSurface;
}

export interface ControlServerHandle {
  /** Actual bound port (resolved from `0` when `auto` was passed). */
  port: number;
  /** Underlying Node HTTP server. Exposed only for tests. */
  server: Server;
  /** Gracefully stop accepting new connections and close. */
  close(): Promise<void>;
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
  return new Promise<ControlServerHandle>((resolve, reject) => {
    const server = createServer((req, res) => {
      handleRequest(req, res, opts).catch((err: unknown) => {
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
        close: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          }),
      };
      resolve(handle);
    });
  });
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
  opts: ControlServerOptions,
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
    const result = await route(req, opts.surface, pathname, method);
    sendJson(res, result.status, result.body);
  } catch (err) {
    if (err instanceof ControlServerError) {
      sendJson(res, err.status, { error: err.message });
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
  surface: OrchestratorControlSurface,
  pathname: string,
  method: string,
): Promise<RouteResult> {
  if (method === "GET" && pathname === "/bots") {
    return listBots(surface);
  }
  if (method === "POST" && pathname === "/launch") {
    const body = await readJsonBody(req);
    return launchOne(surface, body);
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
  if (authBackend !== "jwt" && authBackend !== "storage-state") {
    throw new ControlServerError(400, '"authBackend" must be "jwt" or "storage-state"');
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
