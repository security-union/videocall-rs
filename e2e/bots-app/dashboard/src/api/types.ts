/**
 * TypeScript mirror of the phase-4 control API's wire shapes. Kept in
 * sync by hand with `e2e/bots-app/src/control/registry.ts` and
 * `server.ts`. If those drift, the corresponding `*.test.ts` here
 * will fail to compile and surface the divergence loudly.
 */

export type BotStatus = "launching" | "joining" | "in-meeting" | "leaving" | "done" | "failed";

/**
 * Where the bot is running. `{ kind: "local" }` is the in-process
 * Playwright path; `{ kind: "ssh", hostLabel }` means the bot was
 * launched on a remote machine over SSH using the registry entry
 * `<hostLabel>`. The dashboard's bots-table renders this as a chip
 * (`local` or `ssh:<label>`).
 */
export type BotHostKind = { kind: "local" } | { kind: "ssh"; hostLabel: string };

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
  /**
   * Where the bot is running. Always emitted by current servers; the
   * `?` is back-compat with the older pre-SSH server payload that
   * omits the field — the table renders that as "local".
   */
  host?: BotHostKind;
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
  /**
   * Where the bot's Chrome runs. Pre-SSH the dashboard sent the bare
   * string `"local"` / `"future-ssh"`; the SSH PR introduces the
   * structured form `{ kind: "local" }` / `{ kind: "ssh", hostLabel }`.
   * The Node sidecar accepts both shapes; new clients should use the
   * structured one. The legacy string-only union stays for
   * back-compat with stored profiles and external callers.
   */
  runLocation:
    | "local"
    | "future-vm"
    | "future-ssh"
    | "future-docker"
    | { kind: "local" }
    | { kind: "ssh"; hostLabel: string };
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
  /**
   * Where every spawned bot's Chrome runs. Same shape as the single
   * launch's `runLocation`. v1 has no fan-out — all N bots land on
   * the one chosen host.
   */
  runLocation?: { kind: "local" } | { kind: "ssh"; hostLabel: string };
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

// ──────────────────────────────────────────────────────────────────────
// prep-assets background job — regenerates costume y4m + stitched WAV
// from the conversation manifest, identical to `bots-app prep-assets`.
// ──────────────────────────────────────────────────────────────────────

export interface PrepAssetsStartRequest {
  manifestPath?: string;
  costumeSource?: string;
  outputDir?: string;
  participants?: string[];
}

export interface PrepAssetsStartResponse {
  jobId: string;
  status: "running";
  startedAt: number;
}

export interface PrepAssetsJobStatus {
  jobId: string;
  status: "running" | "done" | "failed";
  startedAt: number;
  finishedAt: number | null;
  stdoutLog: string[];
  exitCode: number | null;
  error: string | null;
  audioPrepped: number;
  costumesPrepped: number;
}

// ──────────────────────────────────────────────────────────────────────
// SSH host registry — per-host CRUD + the `POST /hosts/:label/test`
// probe. Persistence is server-side under `<runDir>/hosts.json`. See
// `e2e/bots-app/src/control/ssh-hosts.ts` for the validation rules.
// ──────────────────────────────────────────────────────────────────────

export interface SshHost {
  label: string;
  host: string;
  user: string;
  sshKey: string | null;
  reposPath: string;
  notes: string | null;
  /**
   * Shell name (`bash`, `zsh`, `sh`) or absolute path used for the
   * outer `<shell> -lc …` wrapper. `null` defaults to `bash`.
   */
  shell: string | null;
  /**
   * Profile file sourced on the remote BEFORE the bot command runs.
   * Emitted as `[ -f <profileFile> ] && . <profileFile>;` so a missing
   * file is a silent no-op. `null` suppresses the source line entirely.
   * Defaults are inferred from {@link shell} client-side: bash →
   * `~/.bash_profile`, zsh → `~/.zshrc`.
   */
  profileFile: string | null;
  /**
   * Free-form pre-command run AFTER sourcing the profile and BEFORE
   * the `cd && npm run …` chain. Used for nvm version pinning, PATH
   * exports, etc. Terminated with `;` in the emitted prefix so a
   * non-zero exit doesn't abort the bot launch.
   * See `e2e/bots-app/src/control/ssh-hosts.ts` for validation rules.
   */
  preCommand: string | null;
  addedAt: number;
}

export interface SshHostsResponse {
  hosts: SshHost[];
}

export interface AddSshHostRequest {
  label: string;
  host: string;
  user?: string;
  sshKey?: string | null;
  reposPath: string;
  notes?: string | null;
  shell?: string | null;
  profileFile?: string | null;
  preCommand?: string | null;
}

export interface UpdateSshHostRequest {
  host?: string;
  user?: string;
  sshKey?: string | null;
  reposPath?: string;
  notes?: string | null;
  shell?: string | null;
  profileFile?: string | null;
  preCommand?: string | null;
}

export interface TestSshHostResponse {
  ok: boolean;
  latencyMs?: number;
  output?: string;
  error?: string;
}

/**
 * Body shape for `POST /api/hosts/:label/preview-launch`. Mirrors the
 * subset of {@link LaunchRequest} fields that affect the constructed
 * SSH command — costume / audio / storage-state-path are deliberately
 * omitted because they do NOT change the remote bash command and would
 * just clutter the wire.
 */
export interface SshPreviewLaunchRequest {
  meetingURL: string;
  participant: string;
  displayName?: string;
  ttl: string;
  headless: boolean;
  network: string;
  authBackend: "jwt" | "storage-state" | "none";
}

/**
 * Response shape for `POST /api/hosts/:label/preview-launch`. The
 * `display` field is a single-line human-readable rendering of the
 * `argv` slot list (each slot single-quoted where needed). The
 * `remoteCommand` is the embedded bash command (the last argv slot)
 * exposed separately so the dashboard can show it on its own if it
 * wants.
 */
export interface SshPreviewLaunchResponse {
  argv: string[];
  display: string;
  remoteCommand: string;
}

/**
 * Body shape for `POST /api/hosts/preview` — preview the SSH command
 * for an UNSAVED host config (used by the Add/Edit Host dialog's live
 * preview so the operator sees what command will run BEFORE saving).
 * The `launchSpec` slot lets callers override the default placeholder
 * tokens (`<participant>`, `<meeting-url>`, etc.) when they have real
 * values to render.
 */
export interface SshPreviewHostRequest {
  host: AddSshHostRequest;
  launchSpec?: {
    meetingURL?: string;
    participant?: string;
    displayName?: string;
    ttl?: string;
    network?: string;
    authBackend?: string;
  };
}

/**
 * Response shape for `POST /api/hosts/preview`. Same triple as
 * {@link SshPreviewLaunchResponse} — the `display` field is the
 * single-line human-readable rendering the dashboard surfaces in
 * the Add/Edit Host dialog's "Sample command" card.
 */
export type SshPreviewHostResponse = SshPreviewLaunchResponse;

export interface BotLogResponse {
  lines: string[];
  totalLines: number;
}
