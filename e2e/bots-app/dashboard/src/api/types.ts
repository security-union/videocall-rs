/**
 * TypeScript mirror of the phase-4 control API's wire shapes. Kept in
 * sync by hand with `e2e/bots-app/src/control/registry.ts` and
 * `server.ts`. If those drift, the corresponding `*.test.ts` here
 * will fail to compile and surface the divergence loudly.
 */

export type BotStatus = "launching" | "joining" | "in-meeting" | "leaving" | "done" | "failed";

export interface BotSnapshot {
  botId: string;
  participant: string;
  status: BotStatus;
  startedAt: number;
  meetingURL: string;
  network: string | null;
  ttl: string;
  ttlRemainingMs: number | null;
  finishReason?: string;
  lastError?: string;
}

export interface BotListResponse {
  bots: BotSnapshot[];
}

export interface HealthResponse {
  ok: boolean;
  bots: number;
}

export interface DaemonInfo {
  pid: number;
  port: number;
  startedAt: string;
  /**
   * `"self-hosted"` — the orchestrator + ctl server were spawned
   * in-process by `bots-app dashboard` itself (the default modern
   * flow). `"attached"` — the dashboard is talking to an external
   * `bots-app run --ctl-port auto` daemon. Older dashboard backends
   * (before phase 5.1) omit the field; the UI treats undefined as
   * `"attached"` for back-compat.
   */
  mode?: "self-hosted" | "attached";
}

/**
 * Body shape for `POST /api/launch`. Mirrors the legacy CLI fields
 * that `bots-app run` accepts. `runLocation` is dashboard-only — the
 * Node sidecar rejects anything other than `"local"` today and the
 * other slots render as disabled-with-tooltip.
 */
export interface LaunchRequest {
  meetingURL: string;
  participant: string;
  displayName?: string;
  ttl: string;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
  /**
   * Absolute path to the HCL SSO storage-state JSON the dashboard
   * has captured (typically `<runDir>/auth/hcl-sso.json`). Forwarded
   * to the orchestrator and consumed only when `authBackend === "jwt"`.
   * Optional: when absent and the file does not exist on the
   * orchestrator's runDir, the bot falls back to the legacy "no SSO
   * state" path (the operator's session must already be valid in the
   * target environment, or the bot will hit the SSO portal).
   */
  ssoStateFile?: string;
  runLocation: "local" | "future-vm" | "future-ssh" | "future-docker";
}

export interface LaunchResponse {
  botId: string;
}

/**
 * Server-persisted run profile metadata (returned by `GET /api/profiles`).
 * The full bot list is only fetched on demand via `GET /api/profiles/:name`.
 */
export interface ProfileSummary {
  name: string;
  savedAt: string;
  botCount: number;
}

export interface ProfileListResponse {
  profiles: ProfileSummary[];
}

/**
 * Per-bot launch spec persisted inside a profile JSON. Mirrors the
 * Node side's `ProfileBotSpec`.
 */
export interface ProfileBotSpec {
  meetingURL: string;
  participant: string;
  displayName?: string;
  ttl: string;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
}

export interface RunProfile {
  name: string;
  savedAt: string;
  version: number;
  bots: ProfileBotSpec[];
}

export interface SaveProfileFromCurrentRequest {
  name: string;
  source: "current";
}

export interface SaveProfileFromBotsRequest {
  name: string;
  source: { bots: ProfileBotSpec[] };
}

export type SaveProfileRequest = SaveProfileFromCurrentRequest | SaveProfileFromBotsRequest;

export interface LaunchProfileResponse {
  name: string;
  botIds: string[];
}

/**
 * Response shape for `GET /api/sso/vpn-status`. The endpoint
 * synthesizes a 5s `fetch` against the HCL host and translates the
 * outcome into one of two statuses.
 */
export type VpnStatusResponse =
  | {
      status: "up";
      checkedAt: number;
      responseTimeMs: number;
      httpStatus?: number;
    }
  | {
      status: "down";
      checkedAt: number;
      error: string;
      responseTimeMs?: number;
    };

/**
 * Response shape for `GET /api/sso/status`. `ageHours` is derived from
 * the file's mtime; cookie expiry is intentionally not inspected.
 */
export interface SsoStatusResponse {
  filePath: string;
  exists: boolean;
  capturedAt: number | null;
  ageHours: number | null;
  size: number | null;
}

export interface SsoRecaptureStartRequest {
  startUrl?: string;
}

export interface SsoRecaptureStartResponse {
  recaptureSessionId: string;
  startUrl: string;
  startedAt: number;
}

export interface SsoRecaptureCancelResponse {
  recaptureSessionId: string;
  cancelled: boolean;
}

/**
 * One row of the participant → costume / audio mapping the
 * `/api/assets/manifest` endpoint returns. The Launch form uses this
 * to auto-default its Costume + Audio dropdowns when the operator
 * types a participant name that has a manifest entry. `null` fields
 * mean either "no manifest match" or "manifest mapping exists but the
 * prep'd file is missing on disk" — the form treats both as "leave the
 * field at its current value".
 */
export interface AssetsManifestParticipant {
  name: string;
  costumeFile: string | null;
  audioFile: string | null;
}

export interface AssetsManifestResponse {
  participants: AssetsManifestParticipant[];
}
