import { existsSync } from "node:fs";
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { join, resolve } from "node:path";

/**
 * SSH host registry. Stores a list of operator-curated remote hosts the
 * dashboard can launch bots against via the local `ssh` binary. The
 * registry lives at `<runDir>/hosts.json` with mode 0o600 — it carries
 * hostnames + usernames + optional private-key paths and should be
 * treated as sensitive (though never actually secret material; the
 * operator's `ssh-agent` + `~/.ssh/config` remain the source of truth
 * for credentials).
 *
 * Wire shape (one row per host):
 *
 *   {
 *     "label":      "lab-mini-7",
 *     "host":       "lab-mini-7.intra:2222",
 *     "user":       "alice",
 *     "sshKey":     "/home/alice/.ssh/id_lab_ed25519",
 *     "reposPath":  "/home/alice/videocall",
 *     "notes":      "lab Mac mini, near the rack",
 *     "addedAt":    1736890123456
 *   }
 *
 * Validation rules (mirrored on the dashboard client-side for early
 * feedback; the server-side checks here are the source of truth):
 *
 *   - label:     `^[A-Za-z0-9][A-Za-z0-9-]{0,62}$`
 *   - host:      no whitespace, no shell metacharacters
 *   - user:      `^[A-Za-z0-9._-]{1,32}$`
 *   - sshKey:    absolute path (starts with `/` or `~`), must exist on
 *                the local FS at `addHost` time
 *   - reposPath: absolute path (starts with `/` or `~`)
 *
 * Persistence is JSON. Reads tolerate a missing file (→ empty list) but
 * NOT malformed JSON (the operator must clean up the file manually —
 * silently dropping persisted hosts on a parse error would lose data).
 */

export const HOSTS_FILENAME = "hosts.json";
export const HOSTS_FILE_MODE = 0o600;
export const HOSTS_SCHEMA_VERSION = 1;
const LABEL_PATTERN = /^[A-Za-z0-9][A-Za-z0-9-]{0,62}$/;
const USER_PATTERN = /^[A-Za-z0-9._-]{1,32}$/;
// Host can be a DNS name, IPv4, or "name:port". We forbid whitespace and
// shell metacharacters up-front so the resulting `<user>@<host>` token
// is safe to plug into an `ssh` argv slot (we use `child_process.spawn`,
// not a shell, but defense-in-depth still applies for misconfigured
// downstream consumers).
const HOST_FORBIDDEN_RE = /[\s'"`$;&|<>(){}\\]/;

export interface SshHost {
  label: string;
  host: string;
  user: string;
  /** Absolute path to a private key, or `null` to rely on ssh-agent. */
  sshKey: string | null;
  reposPath: string;
  notes: string | null;
  addedAt: number;
}

export interface SshHostInput {
  label: string;
  host: string;
  user?: string;
  sshKey?: string | null;
  reposPath: string;
  notes?: string | null;
}

export interface SshHostPatch {
  host?: string;
  user?: string;
  sshKey?: string | null;
  reposPath?: string;
  notes?: string | null;
}

export class SshHostValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SshHostValidationError";
  }
}

export class SshHostExistsError extends Error {
  constructor(label: string) {
    super(`ssh host "${label}" already exists`);
    this.name = "SshHostExistsError";
  }
}

export class SshHostNotFoundError extends Error {
  constructor(label: string) {
    super(`ssh host "${label}" not found`);
    this.name = "SshHostNotFoundError";
  }
}

export interface TestHostResult {
  ok: boolean;
  latencyMs?: number;
  output?: string;
  error?: string;
}

export function hostsFilePath(runDir: string): string {
  return join(runDir, HOSTS_FILENAME);
}

interface PersistedHosts {
  version: number;
  hosts: SshHost[];
}

/**
 * Load the persisted registry. A missing file returns an empty list; a
 * malformed file throws (we never silently drop data).
 */
export async function listHosts(runDir: string): Promise<SshHost[]> {
  const path = hostsFilePath(runDir);
  if (!existsSync(path)) return [];
  const raw = await readFile(path, "utf8");
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new SshHostValidationError(
      `ssh hosts file ${path} is not valid JSON: ${(e as Error).message}`,
    );
  }
  if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new SshHostValidationError(`ssh hosts file ${path} is not a JSON object`);
  }
  const obj = parsed as Record<string, unknown>;
  const version = typeof obj.version === "number" ? obj.version : 0;
  if (version < 1) {
    throw new SshHostValidationError(`ssh hosts file ${path}: "version" must be >= 1`);
  }
  if (!Array.isArray(obj.hosts)) {
    throw new SshHostValidationError(`ssh hosts file ${path}: "hosts" must be an array`);
  }
  return obj.hosts.map((entry, idx) => validateStoredHost(entry, `${path} hosts[${idx}]`));
}

export async function getHost(runDir: string, label: string): Promise<SshHost | null> {
  const all = await listHosts(runDir);
  return all.find((h) => h.label === label) ?? null;
}

/**
 * Validate + persist a new host. Refuses to overwrite (operator must
 * call `updateHost` or `removeHost` first). Returns the canonical row
 * (with `addedAt` resolved).
 */
export async function addHost(runDir: string, spec: SshHostInput): Promise<SshHost> {
  const all = await listHosts(runDir);
  const candidate = buildHost(spec, Date.now());
  if (all.some((h) => h.label === candidate.label)) {
    throw new SshHostExistsError(candidate.label);
  }
  const next = [...all, candidate];
  await persist(runDir, next);
  return candidate;
}

/**
 * Patch an existing host. The label is immutable — operators rename by
 * `removeHost` + `addHost`. Returns the resulting row.
 */
export async function updateHost(
  runDir: string,
  label: string,
  patch: SshHostPatch,
): Promise<SshHost> {
  if (!LABEL_PATTERN.test(label)) {
    throw new SshHostValidationError(`label must match ${LABEL_PATTERN.source} (got "${label}")`);
  }
  const all = await listHosts(runDir);
  const idx = all.findIndex((h) => h.label === label);
  if (idx === -1) {
    throw new SshHostNotFoundError(label);
  }
  const existing = all[idx];
  const merged: SshHost = {
    ...existing,
    host: patch.host !== undefined ? validateHostField(patch.host) : existing.host,
    user: patch.user !== undefined ? validateUserField(patch.user) : existing.user,
    sshKey:
      patch.sshKey === undefined
        ? existing.sshKey
        : patch.sshKey === null || patch.sshKey === ""
          ? null
          : validateSshKeyField(patch.sshKey),
    reposPath:
      patch.reposPath !== undefined ? validateReposPathField(patch.reposPath) : existing.reposPath,
    notes:
      patch.notes === undefined
        ? existing.notes
        : patch.notes === null || patch.notes === ""
          ? null
          : validateNotesField(patch.notes),
  };
  const next = [...all];
  next[idx] = merged;
  await persist(runDir, next);
  return merged;
}

export async function removeHost(runDir: string, label: string): Promise<void> {
  if (!LABEL_PATTERN.test(label)) {
    throw new SshHostValidationError(`label must match ${LABEL_PATTERN.source} (got "${label}")`);
  }
  const all = await listHosts(runDir);
  const idx = all.findIndex((h) => h.label === label);
  if (idx === -1) {
    throw new SshHostNotFoundError(label);
  }
  const next = all.filter((_, i) => i !== idx);
  await persist(runDir, next);
}

/**
 * Probe a host by running `ssh -o ConnectTimeout=5 ... 'echo bots-app-probe-ok && uname -a'`.
 * Resolves to `{ ok: true, latencyMs, output }` on success and
 * `{ ok: false, error }` on any failure (DNS, auth, timeout, unknown).
 *
 * Implementation detail: we spawn the system `ssh` binary directly
 * (no shell). The remote command is a single token passed as the last
 * argv slot, so spaces don't matter.
 */
export async function testHost(
  runDir: string,
  label: string,
  deps: TestHostDeps = {},
): Promise<TestHostResult> {
  const host = await getHost(runDir, label);
  if (host === null) throw new SshHostNotFoundError(label);
  return runSshProbe(host, deps);
}

/**
 * Dependency injection seam for `testHost`. Tests substitute a stub
 * `spawn` so they don't have to actually fork an `ssh` process. The
 * stub must implement the subset of `child_process.spawn` we use
 * (stdout/stderr streams + an `exit` event + an optional `error` event).
 */
export interface TestHostDeps {
  spawn?: typeof spawn;
}

export async function runSshProbe(host: SshHost, deps: TestHostDeps = {}): Promise<TestHostResult> {
  const spawnImpl = deps.spawn ?? spawn;
  const args = buildSshArgsForProbe(host);
  const t0 = Date.now();
  return new Promise<TestHostResult>((resolveFn) => {
    let child;
    try {
      child = spawnImpl("ssh", args, { stdio: ["ignore", "pipe", "pipe"] });
    } catch (e) {
      resolveFn({ ok: false, error: `spawn failed: ${(e as Error).message}` });
      return;
    }
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (b: Buffer) => {
      stdout += b.toString("utf8");
    });
    child.stderr?.on("data", (b: Buffer) => {
      stderr += b.toString("utf8");
    });
    child.on("error", (err: Error) => {
      resolveFn({ ok: false, error: `spawn failed: ${err.message}` });
    });
    child.on("exit", (code: number | null) => {
      const latencyMs = Date.now() - t0;
      if (code === 0 && stdout.includes("bots-app-probe-ok")) {
        resolveFn({ ok: true, latencyMs, output: stdout.trim() });
        return;
      }
      const error =
        stderr.trim() ||
        stdout.trim() ||
        `ssh exited with code ${code === null ? "(killed)" : code}`;
      resolveFn({ ok: false, latencyMs, error });
    });
  });
}

/**
 * Build the argv array for the probe command. Exported for unit tests.
 */
export function buildSshArgsForProbe(host: SshHost): string[] {
  return [...buildBaseSshArgs(host, { connectTimeout: 5 }), "echo bots-app-probe-ok && uname -a"];
}

/**
 * Build the argv for a real bot launch over SSH. The remote command is
 * a single token (single-line bash) — every dynamic substring is
 * shell-escaped via {@link shellEscape}.
 *
 * The inner command is wrapped in `${SHELL:-/bin/bash} -lc <escaped>`
 * so the remote shell runs as a **login shell**. Without `-l` the
 * remote shell is non-interactive non-login and does NOT source the
 * operator's profile (`~/.bash_profile`, `~/.profile`, `~/.zprofile`),
 * which is where modern node installs (nvm, fnm, asdf, homebrew on
 * macOS) put `npm` on PATH. The symptom of skipping this wrapper is a
 * remote-side `bash: npm: command not found`.
 *
 * `$SHELL` expands on the REMOTE side — it stays as a literal token
 * here. The `${SHELL:-/bin/bash}` form falls back to `/bin/bash` on
 * stripped-down hosts where `$SHELL` isn't exported (rare). Using the
 * operator's login shell (rather than hard-coding bash) handles
 * zsh-default macOS hosts whose PATH lives in `~/.zprofile`, which
 * `bash -l` would not source.
 */
export function buildSshArgsForLaunch(host: SshHost, remoteCmd: string): string[] {
  const wrapped = `\${SHELL:-/bin/bash} -lc ${shellEscape(remoteCmd)}`;
  return [...buildBaseSshArgs(host, { connectTimeout: 10 }), wrapped];
}

function buildBaseSshArgs(host: SshHost, opts: { connectTimeout: number }): string[] {
  const args: string[] = [
    "-o",
    `ConnectTimeout=${opts.connectTimeout}`,
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "BatchMode=yes",
  ];
  if (host.sshKey) {
    args.push("-i", host.sshKey);
  }
  // Parse out the optional `:port` suffix so it lands on a `-p` flag
  // rather than getting embedded in the destination token (ssh does
  // not accept `user@host:port` directly).
  const { host: bare, port } = splitHostPort(host.host);
  if (port !== null) {
    args.push("-p", String(port));
  }
  args.push(`${host.user}@${bare}`);
  return args;
}

function splitHostPort(raw: string): { host: string; port: number | null } {
  // IPv6 forms (`[::1]:22`) aren't supported in v1 — operators with
  // IPv6 hosts can lean on `~/.ssh/config` aliases instead.
  const m = /^([^:]+):(\d+)$/.exec(raw);
  if (m) {
    const port = Number.parseInt(m[2], 10);
    if (Number.isFinite(port) && port > 0 && port < 65536) {
      return { host: m[1], port };
    }
  }
  return { host: raw, port: null };
}

/**
 * POSIX single-quote shell escaper. Wraps in single quotes and escapes
 * any internal single-quote via the standard `'\''` dance.
 *
 *   shellEscape("a")          → "'a'"
 *   shellEscape("a'b")        → "'a'\\''b'"
 *   shellEscape("")           → "''"
 *
 * Used to build the remote single-line bash command for a bot launch.
 * Each dynamic substring (meeting URL, participant, network, etc.) is
 * escaped before being concatenated.
 */
export function shellEscape(value: string): string {
  return "'" + value.replace(/'/g, "'\\''") + "'";
}

/**
 * Build the single-line bash command the dashboard tells the remote
 * host to run. The form is:
 *
 *   cd '<reposPath>'/e2e && npm run bot -- run --headless \
 *     --ttl '<ttl>' --meeting-url '<url>' --participant '<p>' \
 *     [--network '<net>'] [--auth '<auth>'] [--display-name '<name>']
 *
 * Every dynamic value is escaped via {@link shellEscape}. The
 * `'<reposPath>'/e2e` form (closing the quote before the literal
 * `/e2e`) keeps the path component escaped while letting the literal
 * `/e2e` extension remain visible — same trick the `cd` builtin uses
 * for path-with-spaces.
 */
export interface RemoteLaunchCmd {
  reposPath: string;
  ttl: string;
  meetingURL: string;
  participant: string;
  network?: string | null;
  authBackend?: string | null;
  displayName?: string | null;
  headless?: boolean;
}

export function buildRemoteLaunchCommand(spec: RemoteLaunchCmd): string {
  const parts: string[] = [];
  parts.push(`cd ${shellEscape(spec.reposPath)}/e2e`);
  const cmd: string[] = ["npm", "run", "bot", "--", "run"];
  if (spec.headless !== false) cmd.push("--headless");
  cmd.push("--ttl", shellEscape(spec.ttl));
  cmd.push("--meeting-url", shellEscape(spec.meetingURL));
  cmd.push("--participant", shellEscape(spec.participant));
  if (spec.network && spec.network !== "none") {
    cmd.push("--network", shellEscape(spec.network));
  }
  if (spec.authBackend) {
    cmd.push("--auth", shellEscape(spec.authBackend));
  }
  if (spec.displayName) {
    cmd.push("--display-name", shellEscape(spec.displayName));
  }
  parts.push(cmd.join(" "));
  return parts.join(" && ");
}

// ──────────────────────────────────────────────────────────────────────
// Internals
// ──────────────────────────────────────────────────────────────────────

async function persist(runDir: string, hosts: SshHost[]): Promise<void> {
  // Make sure the runDir exists. The orchestrator creates it on
  // startup, but the tests use mkdtemp + subdir patterns and we want a
  // graceful fallback.
  await mkdir(runDir, { recursive: true });
  const path = hostsFilePath(runDir);
  const payload: PersistedHosts = {
    version: HOSTS_SCHEMA_VERSION,
    hosts,
  };
  await writeFile(path, JSON.stringify(payload, null, 2), {
    encoding: "utf8",
    mode: HOSTS_FILE_MODE,
  });
  // `writeFile` honors `mode` only when the file is being created. For
  // overwrites of an existing file the permission stays whatever was
  // there before. Call `chmod` unconditionally so an old-mode file
  // gets locked down on the next save.
  try {
    await chmod(path, HOSTS_FILE_MODE);
  } catch {
    // Non-fatal: chmod can fail on filesystems that don't honor POSIX
    // permissions (some Windows mounts, etc.). The file contents are
    // saved either way.
  }
}

function buildHost(spec: SshHostInput, now: number): SshHost {
  return {
    label: validateLabelField(spec.label),
    host: validateHostField(spec.host),
    user: validateUserField(spec.user ?? process.env.USER ?? ""),
    sshKey:
      spec.sshKey === undefined || spec.sshKey === null || spec.sshKey === ""
        ? null
        : validateSshKeyField(spec.sshKey),
    reposPath: validateReposPathField(spec.reposPath),
    notes:
      spec.notes === undefined || spec.notes === null || spec.notes === ""
        ? null
        : validateNotesField(spec.notes),
    addedAt: now,
  };
}

function validateStoredHost(entry: unknown, where: string): SshHost {
  if (entry == null || typeof entry !== "object" || Array.isArray(entry)) {
    throw new SshHostValidationError(`${where} must be an object`);
  }
  const o = entry as Record<string, unknown>;
  const label = expectString(o.label, `${where}.label`);
  const host = expectString(o.host, `${where}.host`);
  const user = expectString(o.user, `${where}.user`);
  const reposPath = expectString(o.reposPath, `${where}.reposPath`);
  const addedAt = expectNumber(o.addedAt, `${where}.addedAt`);
  const sshKey =
    o.sshKey === undefined || o.sshKey === null ? null : expectString(o.sshKey, `${where}.sshKey`);
  const notes =
    o.notes === undefined || o.notes === null ? null : expectString(o.notes, `${where}.notes`);
  // Run the same regex/path checks on the stored row — if a malicious
  // operator hand-edited hosts.json to inject shell metacharacters or
  // a relative path, we refuse to load it.
  return {
    label: validateLabelField(label),
    host: validateHostField(host),
    user: validateUserField(user),
    sshKey: sshKey === null ? null : validateSshKeyField(sshKey, { mustExist: false }),
    reposPath: validateReposPathField(reposPath),
    notes: notes === null ? null : validateNotesField(notes),
    addedAt,
  };
}

function expectString(v: unknown, where: string): string {
  if (typeof v !== "string") {
    throw new SshHostValidationError(`${where} must be a string`);
  }
  return v;
}
function expectNumber(v: unknown, where: string): number {
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new SshHostValidationError(`${where} must be a finite number`);
  }
  return v;
}

export function validateLabelField(raw: string): string {
  if (!LABEL_PATTERN.test(raw)) {
    throw new SshHostValidationError(`label must match ${LABEL_PATTERN.source} (got "${raw}")`);
  }
  return raw;
}

export function validateHostField(raw: string): string {
  if (raw === "") {
    throw new SshHostValidationError(`host must be a non-empty string`);
  }
  if (HOST_FORBIDDEN_RE.test(raw)) {
    throw new SshHostValidationError(
      `host must not contain whitespace or shell metacharacters (got "${raw}")`,
    );
  }
  if (raw.length > 253) {
    throw new SshHostValidationError(`host too long (max 253 chars)`);
  }
  return raw;
}

export function validateUserField(raw: string): string {
  if (!USER_PATTERN.test(raw)) {
    throw new SshHostValidationError(`user must match ${USER_PATTERN.source} (got "${raw}")`);
  }
  return raw;
}

/**
 * sshKey check. By default the key path must exist at validation time
 * (catches typos at `addHost`). The stored-row validator passes
 * `mustExist: false` so a registry entry whose key was later moved
 * doesn't permanently break the dashboard.
 */
export function validateSshKeyField(raw: string, opts: { mustExist?: boolean } = {}): string {
  if (raw === "") {
    throw new SshHostValidationError(`sshKey must be a non-empty path when provided`);
  }
  // Accept absolute paths (`/…`) or home-relative paths (`~…`). The
  // `~` form is intentionally NOT expanded server-side — `ssh -i` does
  // its own tilde expansion, and we want the persisted file to record
  // exactly what the operator typed.
  if (!raw.startsWith("/") && !raw.startsWith("~")) {
    throw new SshHostValidationError(
      `sshKey must be an absolute path (start with "/" or "~"); got "${raw}"`,
    );
  }
  if (opts.mustExist !== false) {
    // Only check existence for absolute paths; tilde-paths would need
    // expansion, which we delegate to `ssh`.
    if (raw.startsWith("/") && !existsSync(raw)) {
      throw new SshHostValidationError(`sshKey path does not exist: ${raw}`);
    }
  }
  return raw;
}

export function validateReposPathField(raw: string): string {
  if (raw === "") {
    throw new SshHostValidationError(`reposPath must be a non-empty path`);
  }
  if (!raw.startsWith("/") && !raw.startsWith("~")) {
    throw new SshHostValidationError(
      `reposPath must be an absolute path (start with "/" or "~"); got "${raw}"`,
    );
  }
  if (HOST_FORBIDDEN_RE.test(raw)) {
    throw new SshHostValidationError(
      `reposPath must not contain whitespace or shell metacharacters`,
    );
  }
  return raw;
}

export function validateNotesField(raw: string): string {
  // Notes are free-text; cap the length and reject embedded NUL.
  if (raw.includes("\0")) {
    throw new SshHostValidationError(`notes must not contain NUL bytes`);
  }
  if (raw.length > 2048) {
    throw new SshHostValidationError(`notes too long (max 2048 chars)`);
  }
  return raw;
}

/**
 * Resolve `<runDir>/hosts.json`. Exposed for tests that want to poke
 * the file directly.
 */
export function resolvedHostsPath(runDir: string): string {
  return resolve(hostsFilePath(runDir));
}
