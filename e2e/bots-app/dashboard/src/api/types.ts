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
