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
  /**
   * Operator-picked costume basename (e.g. `pirate.y4m`) from the
   * launch form's Costume Select. Forwarded to the orchestrator,
   * which composes `<runDir>/costumes/<costume>` and feeds it to
   * Chrome's `--use-file-for-fake-video-capture` flag — overriding
   * the participant-based manifest auto-match. The literal sentinel
   * `"default"` is omitted by the form (the user is asking for
   * Chrome's default fake pattern).
   */
  costume?: string;
  /**
   * Mirror of {@link costume} for the audio side. Basename under
   * `<runDir>/audio/`.
   */
  audio?: string;
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

// ──────────────────────────────────────────────────────────────────────
// Multi-launch (first-N from manifest + random-N) and YAML config import
// ──────────────────────────────────────────────────────────────────────

/**
 * Body shape for `POST /api/launch/multi`. Mirrors the CLI's
 * `bots-app run --users N` (first-N pick) and `bots-app gen` (random
 * pick) flows so dashboard operators can spawn a fleet with one click.
 *
 * `displayNameTemplate` accepts a `{participant}` token that the server
 * substitutes per bot — e.g. `"Bot {participant}"` → `"Bot alice"`,
 * `"Bot bob"`. Untemplated strings become a constant prefix for all
 * bots in the batch.
 */
export interface MultiLaunchRequest {
  mode: "first-n" | "random";
  count: number;
  seed?: number;
  includeObservers?: boolean;
  maxUsers?: number;

  meetingURL: string;
  ttl: string;
  network?: string;
  headless?: boolean;
  authBackend?: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
  ssoStateFile?: string;
  displayNameTemplate?: string;
}

export interface MultiLaunchResponse {
  mode: "first-n" | "random";
  count: number;
  seed: number | null;
  participants: string[];
  botIds: string[];
  errors: Array<{ participant: string; message: string }>;
}

/**
 * Body shape for `POST /api/launch/from-config`. The whole YAML text
 * comes in on `configYaml`; the server parses and validates it the
 * same way the CLI's `--config <path>` flag does.
 */
export interface LaunchFromConfigRequest {
  configYaml: string;
  headless?: boolean;
  authBackend?: "jwt" | "storage-state" | "none";
  storageStateFile?: string;
  ssoStateFile?: string;
}

export interface LaunchFromConfigResponse {
  meetingUrl: string;
  count: number;
  botIds: string[];
  errors: Array<{ index: number; participant?: string; message: string }>;
}

export interface LaunchFromConfigPreviewResponse {
  meetingUrl: string;
  ttl: string | null;
  network: string | null;
  auth: string | null;
  botCount: number;
  bots: Array<{
    participant: string;
    ttl?: string;
    network?: string;
    auth?: string;
  }>;
  meta: { seed?: number; generatedAt?: string } | null;
}

// ──────────────────────────────────────────────────────────────────────
// OAuth session capture (Google OAuth for app.videocall.rs et al.).
// Parallel to the HCL SSO recapture flow above; files live at
// <runDir>/auth/<label>.json. hcl-sso.json is excluded from listings.
// ──────────────────────────────────────────────────────────────────────

export interface OauthSessionInfo {
  label: string;
  filePath: string;
  capturedAt: number;
  ageHours: number;
  size: number;
}

export interface OauthSessionsResponse {
  sessions: OauthSessionInfo[];
}

export interface OauthCaptureStartRequest {
  label: string;
  startUrl?: string;
}

export interface OauthCaptureStartResponse {
  captureSessionId: string;
  label: string;
  startUrl: string;
  startedAt: number;
}

export interface OauthCaptureCancelResponse {
  captureSessionId: string;
  cancelled: boolean;
}
