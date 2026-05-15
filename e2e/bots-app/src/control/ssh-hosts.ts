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
// Per-host shell choice. Bare names like `bash`, `zsh`, `sh` or absolute
// paths like `/opt/homebrew/bin/zsh` are allowed; shell metacharacters
// are rejected so the resulting `<shell> -lc …` token is safe to embed
// in the SSH wrapper.
const SHELL_PATTERN = /^[A-Za-z0-9_/.-]{1,128}$/;
// Profile file (e.g. `~/.bash_profile`, `~/.zshrc`). Allows `~` prefix,
// alphanumerics, `_`, `.`, `-`, and `/`. Rejects whitespace and shell
// metacharacters to keep the `[ -f <path> ] && . <path>` source line safe.
const PROFILE_FILE_PATTERN = /^[~/A-Za-z0-9_./-]{1,256}$/;
/**
 * Default shell name when the host's `shell` field is unset. `bash`
 * gives us a POSIX-defined login-shell init chain that reliably reads
 * `~/.bash_profile` regardless of the operator's default shell.
 */
export const DEFAULT_SHELL = "bash";

export interface SshHost {
  label: string;
  host: string;
  user: string;
  /** Absolute path to a private key, or `null` to rely on ssh-agent. */
  sshKey: string | null;
  reposPath: string;
  notes: string | null;
  /**
   * Shell to run on the remote host. Either a bare shell name
   * (`bash`, `zsh`, `sh`) or an absolute path. The outer SSH wrapper
   * becomes `<shell> -lc '<inner>'`. When `null` defaults to {@link
   * DEFAULT_SHELL} (`bash`) — bash has a POSIX-defined login-shell
   * init chain that reliably sources `~/.bash_profile`.
   */
  shell: string | null;
  /**
   * Profile file the remote shell sources BEFORE the bot command runs.
   * Emitted as `[ -f <profileFile> ] && . <profileFile>;` so a missing
   * file is a silent no-op. When `null` no source line is emitted.
   * Common defaults inferred client-side: `~/.bash_profile` for bash,
   * `~/.zshrc` for zsh.
   */
  profileFile: string | null;
  /**
   * Optional pre-command string included AFTER sourcing the profile
   * and BEFORE the `cd && npm run …` chain. Free-form bash; we trust
   * the operator (they already have full shell access on the remote).
   *
   * Examples:
   *   ". ~/.nvm/nvm.sh && nvm use 22"
   *   "export PATH=$HOME/.local/bin:$PATH"
   *
   * Terminated with `;` in the emitted prefix so a non-zero exit
   * doesn't abort the bot launch.
   */
  preCommand: string | null;
  addedAt: number;
}

export interface SshHostInput {
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

export interface SshHostPatch {
  host?: string;
  user?: string;
  sshKey?: string | null;
  reposPath?: string;
  notes?: string | null;
  shell?: string | null;
  profileFile?: string | null;
  preCommand?: string | null;
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
    shell:
      patch.shell === undefined
        ? existing.shell
        : patch.shell === null || patch.shell === ""
          ? null
          : validateShellField(patch.shell),
    profileFile:
      patch.profileFile === undefined
        ? existing.profileFile
        : patch.profileFile === null || patch.profileFile === ""
          ? null
          : validateProfileFileField(patch.profileFile),
    preCommand:
      patch.preCommand === undefined
        ? existing.preCommand
        : patch.preCommand === null || patch.preCommand === ""
          ? null
          : validatePreCommandField(patch.preCommand),
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
 * The inner command is wrapped in `<shell> -lc <escaped>` so the remote
 * runs as a **login shell**. `<shell>` is the host's `shell` field
 * (default `bash`). Operators on zsh-default macOS hosts whose nvm
 * lives in `~/.bash_profile` should leave `shell = bash` (the default);
 * operators whose PATH lives in `~/.zshrc` can pick `shell = zsh` and
 * set `profileFile = ~/.zshrc`.
 *
 * The prefix prepended to the cd/npm chain is:
 *
 *   <[ -f profileFile ] && . profileFile>;  <preCommand>;
 *
 * Either half may be omitted when the corresponding host field is
 * null/empty. Both clauses are terminated with `;` (not `&&`) so the
 * rest of the chain runs even when the prior command exits non-zero.
 *
 * See {@link buildRemoteCommandPrefix} for the prefix builder details.
 */
export function buildSshArgsForLaunch(host: SshHost, remoteCmd: string): string[] {
  const prefix = buildRemoteCommandPrefix(host);
  const inner = `${prefix}${remoteCmd}`;
  const shell = host.shell !== null && host.shell !== "" ? host.shell : DEFAULT_SHELL;
  const wrapped = `${shell} -lc ${shellEscape(inner)}`;
  return [...buildBaseSshArgs(host, { connectTimeout: 10 }), wrapped];
}

/**
 * Build the remote-command prefix prepended to the `cd <reposPath>/e2e
 * && npm run …` chain. The shape is `<source>; <preCommand>; ` where:
 *
 *   - `<source>` = `[ -f <profileFile> ] && . <profileFile>` when
 *     `host.profileFile` is set; omitted otherwise.
 *   - `<preCommand>` = `host.preCommand` literal; omitted when null.
 *
 * Returns an empty string when neither field is set. The trailing
 * space-after-semicolon shape keeps the joined result readable when
 * concatenated with the cd/npm chain.
 *
 * Examples:
 *
 *   profileFile=~/.bash_profile, preCommand=null
 *     → "[ -f ~/.bash_profile ] && . ~/.bash_profile; "
 *
 *   profileFile=~/.zshrc, preCommand=". ~/.nvm/nvm.sh && nvm use 22"
 *     → "[ -f ~/.zshrc ] && . ~/.zshrc; . ~/.nvm/nvm.sh && nvm use 22; "
 *
 *   profileFile=null, preCommand=null
 *     → ""
 */
export function buildRemoteCommandPrefix(host: SshHost): string {
  const parts: string[] = [];
  if (host.profileFile !== null && host.profileFile !== "") {
    parts.push(`[ -f ${host.profileFile} ] && . ${host.profileFile}`);
  }
  if (host.preCommand !== null && host.preCommand !== "") {
    // Trim any trailing terminators the operator may have typed.
    parts.push(host.preCommand.replace(/[\s;&]+$/, ""));
  }
  if (parts.length === 0) return "";
  return parts.join("; ") + "; ";
}

/**
 * Compute the default profile file for a given shell name. Applied
 * client-side as a hint when the operator picks a shell in the Add
 * Host dialog; persistence accepts whatever the operator submits
 * (including `null` to suppress the source line entirely).
 *
 * Returns `null` for shells without a well-known convention (POSIX
 * `sh`, custom absolute paths).
 */
export function defaultProfileFileForShell(shell: string | null): string | null {
  if (shell === null || shell === "") return "~/.bash_profile";
  if (shell === "bash") return "~/.bash_profile";
  if (shell === "zsh") return "~/.zshrc";
  return null;
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

/**
 * Validate + materialize a host row from an input spec WITHOUT
 * persisting. Used by the `/hosts/preview` endpoint to validate
 * unsaved host configs the same way `addHost` would. Reuses
 * {@link buildHost} so the validation rules can never drift.
 *
 * The `addedAt` timestamp is set to `0` since the row never lands on
 * disk; callers that need a real timestamp should call `addHost`
 * instead.
 */
export function buildHostForPreview(spec: SshHostInput): SshHost {
  return buildHost(spec, 0);
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
    shell:
      spec.shell === undefined || spec.shell === null || spec.shell === ""
        ? null
        : validateShellField(spec.shell),
    profileFile:
      spec.profileFile === undefined || spec.profileFile === null || spec.profileFile === ""
        ? null
        : validateProfileFileField(spec.profileFile),
    preCommand:
      spec.preCommand === undefined || spec.preCommand === null || spec.preCommand === ""
        ? null
        : validatePreCommandField(spec.preCommand),
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
  // Structured shell config. `shell`, `profileFile`, `preCommand` are
  // forward-compat optional fields — older registries (and any host
  // file the operator may have created during the previous `shellInit`
  // iteration of this PR) simply lack the keys, which load as `null`.
  // Any unknown fields (including legacy `shellInit`) are silently
  // ignored — no migration logic, just drop and move on.
  const shell =
    o.shell === undefined || o.shell === null ? null : expectString(o.shell, `${where}.shell`);
  const profileFile =
    o.profileFile === undefined || o.profileFile === null
      ? null
      : expectString(o.profileFile, `${where}.profileFile`);
  const preCommand =
    o.preCommand === undefined || o.preCommand === null
      ? null
      : expectString(o.preCommand, `${where}.preCommand`);
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
    shell: shell === null || shell === "" ? null : validateShellField(shell),
    profileFile:
      profileFile === null || profileFile === "" ? null : validateProfileFileField(profileFile),
    preCommand:
      preCommand === null || preCommand === "" ? null : validatePreCommandField(preCommand),
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
 * Validate the remote `shell` field. Accepts bare shell names
 * (`bash`, `zsh`, `sh`, `fish`) or absolute paths
 * (`/opt/homebrew/bin/zsh`). Rejects shell metacharacters so the
 * `<shell> -lc …` wrapper is safe to embed.
 */
export function validateShellField(raw: string): string {
  if (raw === "") {
    throw new SshHostValidationError(`shell must be a non-empty string when provided`);
  }
  if (raw.includes("\0")) {
    throw new SshHostValidationError(`shell must not contain NUL bytes`);
  }
  if (!SHELL_PATTERN.test(raw)) {
    throw new SshHostValidationError(`shell must match ${SHELL_PATTERN.source} (got "${raw}")`);
  }
  return raw;
}

/**
 * Validate the remote `profileFile` field. Accepts `~`-prefixed or
 * absolute paths (`~/.bash_profile`, `~/.zshrc`, `/etc/profile`).
 * Rejects shell metacharacters and whitespace so the
 * `[ -f <path> ] && . <path>` source line is safe to embed.
 */
export function validateProfileFileField(raw: string): string {
  if (raw === "") {
    throw new SshHostValidationError(`profileFile must be a non-empty string when provided`);
  }
  if (raw.includes("\0")) {
    throw new SshHostValidationError(`profileFile must not contain NUL bytes`);
  }
  if (!PROFILE_FILE_PATTERN.test(raw)) {
    throw new SshHostValidationError(
      `profileFile must match ${PROFILE_FILE_PATTERN.source} (got "${raw}")`,
    );
  }
  return raw;
}

/**
 * Validate a free-form `preCommand` snippet. The field is
 * operator-supplied bash that runs after sourcing the profile and
 * before the `cd && npm` chain, so we do not try to sandbox it — the
 * operator already has full shell access to the remote via the SSH
 * login they configured. We only reject input that is obviously
 * garbage: NUL bytes, newlines/CRs (the launch command is single-line
 * bash; embedded newlines would break the argv-quoting contract), and
 * overlong values.
 *
 * Maximum length is 512 chars — enough for a chained
 * `. ~/.nvm/nvm.sh && nvm use 22 && export PATH=…` recipe but
 * nowhere near enough to hide a full payload.
 */
export function validatePreCommandField(raw: string): string {
  if (raw === "") {
    throw new SshHostValidationError(`preCommand must be a non-empty string when provided`);
  }
  if (raw.includes("\0")) {
    throw new SshHostValidationError(`preCommand must not contain NUL bytes`);
  }
  if (/[\r\n]/.test(raw)) {
    throw new SshHostValidationError(
      `preCommand must not contain newline or carriage-return characters`,
    );
  }
  if (raw.length > 512) {
    throw new SshHostValidationError(`preCommand too long (max 512 chars)`);
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
