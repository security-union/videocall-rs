import { randomBytes } from "node:crypto";
import { mkdir, writeFile, readFile, readdir, stat } from "node:fs/promises";
import { dirname, join } from "node:path";

/**
 * Generate a fresh control-API bearer token. 32 bytes of CSPRNG entropy,
 * rendered as hex (64 chars). Per-process and never persisted beyond the
 * token file written by {@link writeTokenFile}.
 */
export function generateToken(): string {
  return randomBytes(32).toString("hex");
}

/**
 * Shape of the JSON dropped into `run/ctl-<pid>.token` so the `ctl`
 * client (in the same checkout) can rediscover the running orchestrator
 * without prompting. Fields:
 *   - `port` — TCP port the control HTTP server is bound to
 *   - `token` — bearer token; required on every request except
 *     `/healthz`
 *   - `startedAt` — ISO-8601 timestamp of when the orchestrator booted.
 *     Useful in `ctl` to disambiguate multiple stale token files (we
 *     just pick the newest by mtime, but the field is logged too).
 *   - `pid` — orchestrator process id. Reserved for a future
 *     "stale-token cleanup on next startup" pass; not yet consumed.
 */
export interface TokenFileContents {
  port: number;
  token: string;
  startedAt: string;
  pid: number;
}

/**
 * Default token filename. Per-pid so concurrent `bots-app run` invocations
 * each get their own file and don't clobber each other.
 */
export function defaultTokenFilePath(runDir: string, pid: number = process.pid): string {
  return join(runDir, `ctl-${pid}.token`);
}

/**
 * Write the token file atomically with mode `0o600` (owner read/write
 * only). The directory is created if missing. Any reader without
 * permission to the file is shut out by the filesystem before the
 * HTTP-layer auth check ever runs.
 *
 * The write is "atomic enough" for our purposes — we write to the
 * final path with `mode: 0o600` via `writeFile`. Node's `writeFile`
 * truncates then writes; a partial read by `ctl` during startup is
 * still acceptable because `ctl` retries the file lookup. We do NOT
 * write-then-rename because the `ctl` client only ever reads after
 * the orchestrator has logged "ctl listening" — i.e. *after* the file
 * has been fully flushed.
 */
export async function writeTokenFile(path: string, contents: TokenFileContents): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, JSON.stringify(contents, null, 2), {
    encoding: "utf8",
    mode: 0o600,
  });
}

/**
 * Parse-and-validate the token file at `path`. Throws on malformed
 * JSON, missing fields, or wrong types — the `ctl` client surfaces
 * the error as a "couldn't read token file" diagnostic rather than
 * silently falling through to a bad-auth request.
 */
export async function readTokenFile(path: string): Promise<TokenFileContents> {
  const raw = await readFile(path, "utf8");
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new Error(`token file ${path} is not valid JSON: ${(e as Error).message}`, {
      cause: e,
    });
  }
  if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error(`token file ${path} is not a JSON object`);
  }
  const obj = parsed as Record<string, unknown>;
  if (typeof obj.port !== "number" || !Number.isInteger(obj.port) || obj.port <= 0) {
    throw new Error(`token file ${path}: "port" must be a positive integer`);
  }
  if (typeof obj.token !== "string" || obj.token.length === 0) {
    throw new Error(`token file ${path}: "token" must be a non-empty string`);
  }
  if (typeof obj.startedAt !== "string") {
    throw new Error(`token file ${path}: "startedAt" must be a string`);
  }
  if (typeof obj.pid !== "number") {
    throw new Error(`token file ${path}: "pid" must be a number`);
  }
  return {
    port: obj.port,
    token: obj.token,
    startedAt: obj.startedAt,
    pid: obj.pid,
  };
}

/**
 * Scan `runDir` for `ctl-*.token` files and return the most recently
 * modified one (by mtime). `null` if no token files are present —
 * the `ctl` client surfaces that as a clear "no running orchestrator
 * found" message instead of a cryptic file-not-found from a later
 * `readTokenFile` call.
 */
export async function findLatestTokenFile(runDir: string): Promise<string | null> {
  let entries: string[];
  try {
    entries = await readdir(runDir);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw e;
  }
  const matches = entries.filter((f) => /^ctl-\d+\.token$/.test(f));
  if (matches.length === 0) return null;

  type Sortable = { path: string; mtimeMs: number };
  const stats: Sortable[] = await Promise.all(
    matches.map(async (f) => {
      const p = join(runDir, f);
      const s = await stat(p);
      return { path: p, mtimeMs: s.mtimeMs };
    }),
  );
  stats.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return stats[0].path;
}

/**
 * Constant-time-ish string comparison. Node's `crypto.timingSafeEqual`
 * requires equal-length buffers, which leaks length up front; we
 * compare lengths separately first (a leak we tolerate — the token is
 * always 64 hex chars) then byte-by-byte without short-circuiting.
 */
export function tokensMatch(expected: string, supplied: string): boolean {
  if (expected.length !== supplied.length) return false;
  let diff = 0;
  for (let i = 0; i < expected.length; i++) {
    diff |= expected.charCodeAt(i) ^ supplied.charCodeAt(i);
  }
  return diff === 0;
}

/**
 * Extract the bearer token from a (case-insensitive) `Authorization`
 * header value. Returns `null` when the header is absent or doesn't
 * begin with `Bearer ` (case-insensitive).
 */
export function extractBearerToken(headerValue: string | string[] | undefined): string | null {
  if (headerValue === undefined) return null;
  const value = Array.isArray(headerValue) ? headerValue[0] : headerValue;
  if (typeof value !== "string") return null;
  const match = /^Bearer\s+(\S+)\s*$/i.exec(value);
  return match ? match[1] : null;
}
