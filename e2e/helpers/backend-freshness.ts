import * as fs from "node:fs";
import * as path from "node:path";

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const COMPOSE_FILE = path.resolve(REPO_ROOT, "docker", "docker-compose.e2e.yaml");
const STAMP_DIR = path.resolve(REPO_ROOT, "e2e", ".stack-stamps");

/** Four missed 30s heartbeats from `docker/e2e-backend.sh`. */
const HEARTBEAT_STALE_MS = 120_000;
const STALE_GRACE_MS = 60_000;
const MAX_WAIT_MS = 600_000;
const POLL_INTERVAL_MS = 2_000;

const SKIP_ENV = "E2E_SKIP_BACKEND_FRESHNESS";

export type StampState = "building" | "ok" | "failed";

export interface BackendStamp {
  service: string;
  build: StampState;
  at: string;
}

export interface SupervisedBackend {
  /** Compose service (`websocket-api`) vs cargo bin (`websocket_server`). */
  service: string;
  bin: string;
}

/** The compose file defines the backends, so reading it cannot go stale. */
export function parseSupervisedBackends(composeText: string): SupervisedBackend[] {
  const found = new Map<string, SupervisedBackend>();
  let service = "";
  for (const line of composeText.split("\n")) {
    const header = line.match(/^ {2}([A-Za-z0-9_-]+):\s*$/);
    if (header) service = header[1];
    const cmd = line.match(/e2e-backend\.sh\s+supervise\s+([\w-]+)/);
    if (cmd && service) found.set(cmd[1], { service, bin: cmd[1] });
  }
  return [...found.values()];
}

export function readStamp(stampPath: string): { stamp?: BackendStamp; ageMs?: number } {
  let raw: string;
  let mtimeMs: number;
  try {
    raw = fs.readFileSync(stampPath, "utf8");
    mtimeMs = fs.statSync(stampPath).mtimeMs;
  } catch {
    return {};
  }
  try {
    return { stamp: JSON.parse(raw) as BackendStamp, ageMs: Date.now() - mtimeMs };
  } catch {
    return { ageMs: Date.now() - mtimeMs };
  }
}

export function classifyStamp(
  backend: SupervisedBackend,
  read: { stamp?: BackendStamp; ageMs?: number },
  elapsedMs: number,
): { verdict: "ok" | "wait" | "fail"; reason?: string } {
  const { service, bin } = backend;
  const { stamp, ageMs } = read;
  // Ahead of the build state AND the parse result: a cold stamp describes a
  // container that is gone. `ageMs` is set only when the file exists.
  if (ageMs !== undefined && ageMs > HEARTBEAT_STALE_MS) {
    if (elapsedMs <= STALE_GRACE_MS) {
      return { verdict: "wait", reason: `${bin}'s stamp is from an earlier run` };
    }
    return {
      verdict: "fail",
      reason:
        `${bin}'s stamp has not been touched for ${Math.round(ageMs / 1000)}s, ` +
        `so no watcher is supervising it — the binary it serves may predate your source (#2513).\n` +
        `  make e2e-up   (recreates ${service} with the supervisor)`,
    };
  }
  if (!stamp) {
    return {
      verdict: "wait",
      reason:
        ageMs === undefined
          ? `no stamp yet for ${bin}`
          : `stamp for ${bin} is present but unparseable`,
    };
  }
  if (stamp.build === "failed") {
    return {
      verdict: "fail",
      reason:
        `${bin} FAILED TO BUILD, so the stack is not serving it.\n` +
        `  docker compose -p videocall-e2e -f docker/docker-compose.e2e.yaml logs ${service}\n` +
        `Fix the compile error; the watcher rebuilds on save, no restart needed.`,
    };
  }
  if (stamp.build === "ok") return { verdict: "ok" };
  return { verdict: "wait", reason: `${bin} is rebuilding` };
}

/**
 * Proves a live watcher is in charge of each backend and its last build
 * succeeded (#2513) — not that the binary matches byte for byte, which is
 * cargo's job. Service liveness is left to the HTTP probes that follow.
 */
export async function assertBackendFreshness(): Promise<void> {
  if (process.env[SKIP_ENV]) {
    console.warn(
      `\n!!! ${SKIP_ENV} is set — backend staleness checking is OFF.\n` +
        `!!! A relay built from older source will produce results that look like\n` +
        `!!! real failures (#2513). Unset it before trusting any receipt.\n`,
    );
    return;
  }

  let composeText: string;
  try {
    composeText = fs.readFileSync(COMPOSE_FILE, "utf8");
  } catch (err) {
    throw new Error(`Cannot read ${COMPOSE_FILE} to determine backend services`, { cause: err });
  }

  const backends = parseSupervisedBackends(composeText);
  if (backends.length === 0) {
    throw new Error(
      `No supervised backend services found in ${COMPOSE_FILE}.\n` +
        `Expected at least one 'e2e-backend.sh supervise <bin>' command. If the ` +
        `compose file legitimately no longer uses the supervisor, delete this check ` +
        `rather than letting it pass vacuously (#2513).`,
    );
  }

  const started = Date.now();
  const deadline = started + MAX_WAIT_MS;
  for (const backend of backends) {
    const stampPath = path.resolve(STAMP_DIR, `${backend.bin}.json`);
    let last = "";
    for (;;) {
      const { verdict, reason } = classifyStamp(
        backend,
        readStamp(stampPath),
        Date.now() - started,
      );
      if (verdict === "ok") {
        console.log(`Backend ${backend.service} (${backend.bin}): supervised build ok`);
        break;
      }
      if (verdict === "fail") {
        throw new Error(`E2E backend freshness check failed (#2513).\n${reason}`);
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `E2E backend freshness check timed out after ${MAX_WAIT_MS / 1000}s (#2513).\n` +
            `  Last state: ${reason}\n` +
            `  Expected stamp: ${stampPath}\n` +
            `A stack started before this check existed never writes one — recreate it:\n` +
            `  make e2e-up`,
        );
      }
      if (reason && reason !== last) {
        console.log(`Waiting for backend ${backend.service}: ${reason}...`);
        last = reason;
      }
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
    }
  }
}
