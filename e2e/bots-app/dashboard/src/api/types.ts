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
  authBackend: "jwt" | "storage-state";
  storageStateFile?: string;
  runLocation: "local" | "future-vm" | "future-ssh" | "future-docker";
}

export interface LaunchResponse {
  botId: string;
}
