import type {
  AssetsManifestResponse,
  BotListResponse,
  BotSnapshot,
  DaemonInfo,
  HealthResponse,
  LaunchProfileResponse,
  LaunchRequest,
  LaunchResponse,
  ProfileListResponse,
  RunProfile,
  SaveProfileRequest,
  SsoRecaptureCancelResponse,
  SsoRecaptureStartRequest,
  SsoRecaptureStartResponse,
  SsoStatusResponse,
  VpnStatusResponse,
} from "./types";

/**
 * Browser-side API client. Every call goes through the same-origin
 * `/api/*` proxy served by the Node sidecar — the proxy injects the
 * `Authorization: Bearer <token>` header server-side, so the
 * ctl-API bearer token NEVER reaches the browser. See the
 * `dashboard.ts` server module for the proxy implementation and
 * `../README.md` for the full security model.
 */

export class DashboardApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
    public readonly body?: unknown,
  ) {
    super(message);
    this.name = "DashboardApiError";
  }
}

async function request<T>(
  method: string,
  path: string,
  body?: Record<string, unknown>,
): Promise<T> {
  const init: RequestInit = {
    method,
    headers: { accept: "application/json" },
  };
  if (body !== undefined) {
    init.body = JSON.stringify(body);
    (init.headers as Record<string, string>)["content-type"] = "application/json";
  }
  const res = await fetch(path, init);
  const text = await res.text();
  let parsed: unknown = null;
  if (text.length > 0) {
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = text;
    }
  }
  if (!res.ok) {
    const msg =
      parsed && typeof parsed === "object" && "error" in parsed
        ? String((parsed as { error: unknown }).error)
        : `HTTP ${res.status}`;
    throw new DashboardApiError(res.status, msg, parsed);
  }
  return parsed as T;
}

export const api = {
  daemon: (): Promise<DaemonInfo> => request<DaemonInfo>("GET", "/api/daemon"),
  health: (): Promise<HealthResponse> => request<HealthResponse>("GET", "/api/healthz"),
  listBots: (): Promise<BotListResponse> => request<BotListResponse>("GET", "/api/bots"),
  getBot: (botId: string): Promise<BotSnapshot> =>
    request<BotSnapshot>("GET", `/api/bots/${encodeURIComponent(botId)}`),
  launch: (req: LaunchRequest): Promise<LaunchResponse> =>
    request<LaunchResponse>("POST", "/api/launch", req as unknown as Record<string, unknown>),
  leave: (botId: string): Promise<{ botId: string; action: string }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/leave`),
  kill: (botId: string): Promise<{ botId: string; action: string }> =>
    request("DELETE", `/api/bots/${encodeURIComponent(botId)}`),
  setTtl: (
    botId: string,
    body: { ttl?: string; extendBy?: string },
  ): Promise<{ botId: string; ttl: string }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/ttl`, body),
  setNetwork: (
    botId: string,
    network: string,
  ): Promise<{ botId: string; network: string; note?: string }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/network`, { network }),
  setMic: (botId: string, mic: boolean): Promise<{ botId: string; mic: boolean }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/mute`, { mic }),
  setCamera: (botId: string, camera: boolean): Promise<{ botId: string; camera: boolean }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/video`, { camera }),
  setShare: (botId: string, share: boolean): Promise<{ botId: string; share: boolean }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/share`, { share }),
  duplicate: (
    botId: string,
    overrides: { participant?: string; ttl?: string; network?: string },
  ): Promise<{ botId: string }> =>
    request("POST", `/api/bots/${encodeURIComponent(botId)}/duplicate`, overrides),

  // ──────────────────────────────────────────────────────────────────
  // Run profiles (phase 5.1)
  // ──────────────────────────────────────────────────────────────────
  listProfiles: (): Promise<ProfileListResponse> =>
    request<ProfileListResponse>("GET", "/api/profiles"),
  getProfile: (name: string): Promise<RunProfile> =>
    request<RunProfile>("GET", `/api/profiles/${encodeURIComponent(name)}`),
  saveProfile: (req: SaveProfileRequest): Promise<RunProfile> =>
    request<RunProfile>("POST", "/api/profiles", req as unknown as Record<string, unknown>),
  launchProfile: (name: string): Promise<LaunchProfileResponse> =>
    request<LaunchProfileResponse>(
      "POST",
      `/api/profiles/${encodeURIComponent(name)}/launch`,
    ),
  deleteProfile: (name: string): Promise<{ name: string; deleted: boolean }> =>
    request("DELETE", `/api/profiles/${encodeURIComponent(name)}`),

  // ──────────────────────────────────────────────────────────────────
  // HCL VPN + SSO state management (feat/bots-app-dashboard-sso)
  // ──────────────────────────────────────────────────────────────────
  vpnStatus: (): Promise<VpnStatusResponse> =>
    request<VpnStatusResponse>("GET", "/api/sso/vpn-status"),
  ssoStatus: (): Promise<SsoStatusResponse> =>
    request<SsoStatusResponse>("GET", "/api/sso/status"),
  ssoRecaptureStart: (req: SsoRecaptureStartRequest = {}): Promise<SsoRecaptureStartResponse> =>
    request<SsoRecaptureStartResponse>(
      "POST",
      "/api/sso/recapture",
      req as unknown as Record<string, unknown>,
    ),
  ssoRecaptureComplete: (sessionId: string): Promise<SsoStatusResponse> =>
    request<SsoStatusResponse>(
      "POST",
      `/api/sso/recapture/${encodeURIComponent(sessionId)}/complete`,
    ),
  ssoRecaptureCancel: (sessionId: string): Promise<SsoRecaptureCancelResponse> =>
    request<SsoRecaptureCancelResponse>(
      "DELETE",
      `/api/sso/recapture/${encodeURIComponent(sessionId)}`,
    ),

  // ──────────────────────────────────────────────────────────────────
  // Participant → costume / audio mapping for the Launch form's
  // auto-default behavior.
  // ──────────────────────────────────────────────────────────────────
  assetsManifest: (): Promise<AssetsManifestResponse> =>
    request<AssetsManifestResponse>("GET", "/api/assets/manifest"),
};
