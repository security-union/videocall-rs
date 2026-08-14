import { request } from "node:http";

/**
 * Configuration for a `bots-app ctl` HTTP request.
 *
 * `host` defaults to `127.0.0.1` (the historical hard-coded value) so
 * every existing caller is unaffected. It is overridable so an
 * in-cluster conductor can target a specific bot pod by its
 * StatefulSet DNS name (e.g.
 * `videocall-bots-3.videocall-bots.bot-load.svc`) — the control server
 * on that pod must have been started with a non-loopback `--ctl-bind`
 * and a shared token for this to succeed.
 */
export interface CtlClientConfig {
  /** Target host. Defaults to `127.0.0.1` when unset. */
  host?: string;
  port: number;
  token: string;
  /**
   * Per-request timeout (ms). Defaults to {@link DEFAULT_CTL_TIMEOUT_MS}. On
   * expiry the whole request — INCLUDING the TCP connect phase — is aborted and
   * the promise REJECTS (never hangs). The bound is enforced with
   * `AbortSignal.timeout`; `req.setTimeout` alone would NOT suffice because it is
   * a socket-INACTIVITY timer that does not fire at the requested `timeoutMs`
   * during a hung connect — it trips (if at all) only at the platform's
   * multi-second connect boundary, never at the configured value. This matters
   * for the in-cluster conductor: a pod that is still booting, evicted,
   * node-partitioned, or shaped by a netem profile it can't clear would
   * otherwise blackhole the connection with no `error` event (an unrouted host
   * on macOS, or an unanswered SYN retried for the kernel's full budget on
   * Linux), and the conductor's resolve-retry — which only re-fires on a
   * REJECTION — would never trigger.
   */
  timeoutMs?: number;
}

/** Host used when a {@link CtlClientConfig} omits `host`. */
export const DEFAULT_CTL_HOST = "127.0.0.1";

/**
 * Default per-request timeout. A control call is a small HTTP round-trip to an
 * in-cluster pod (or localhost), so 10s is generous headroom while still
 * bounding a hang. Override via {@link CtlClientConfig.timeoutMs}.
 */
export const DEFAULT_CTL_TIMEOUT_MS = 10_000;

/**
 * HTTP-level error surfaced to the CLI when the control server
 * returns a non-2xx response. `status` is the HTTP status code,
 * `body` is the parsed JSON body (an `{ error: "..." }` shape when
 * the server raised it, the raw string otherwise).
 */
export class CtlHttpError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: unknown,
    message?: string,
  ) {
    super(message ?? `ctl: HTTP ${status} ${formatBody(body)}`);
    this.name = "CtlHttpError";
  }
}

function formatBody(body: unknown): string {
  if (body == null) return "";
  if (
    typeof body === "object" &&
    "error" in body &&
    typeof (body as { error: unknown }).error === "string"
  ) {
    return (body as { error: string }).error;
  }
  return JSON.stringify(body);
}

/**
 * Generic JSON request helper. Sends `body` (when given) as
 * `application/json` and parses the response body as JSON. Throws
 * `CtlHttpError` on non-2xx. Used by every subcommand.
 */
export async function ctlRequest<T = unknown>(
  config: CtlClientConfig,
  method: string,
  path: string,
  body?: Record<string, unknown>,
): Promise<T> {
  const payload = body !== undefined ? JSON.stringify(body) : null;
  const headers: Record<string, string> = {
    accept: "application/json",
    authorization: `Bearer ${config.token}`,
  };
  if (payload !== null) {
    headers["content-type"] = "application/json";
    headers["content-length"] = String(Buffer.byteLength(payload));
  }
  const host = config.host ?? DEFAULT_CTL_HOST;
  const timeoutMs = config.timeoutMs ?? DEFAULT_CTL_TIMEOUT_MS;
  return new Promise<T>((resolve, reject) => {
    const req = request(
      {
        host,
        port: config.port,
        method,
        path,
        headers,
        // Bounds the ENTIRE request, including the TCP connect phase, so an
        // unreachable/blackholing host rejects at `timeoutMs` instead of hanging
        // for the platform's SYN-retry budget (see CtlClientConfig.timeoutMs).
        // `req.setTimeout` is a socket-inactivity timer that does not fire at
        // `timeoutMs` during a hung connect (only at the platform's multi-second
        // connect boundary); AbortSignal.timeout does.
        signal: AbortSignal.timeout(timeoutMs),
      },
      (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (c: Buffer) => chunks.push(c));
        res.on("end", () => {
          const raw = Buffer.concat(chunks).toString("utf8");
          let parsed: unknown = null;
          if (raw.length > 0) {
            try {
              parsed = JSON.parse(raw);
            } catch {
              parsed = raw;
            }
          }
          const status = res.statusCode ?? 0;
          if (status < 200 || status >= 300) {
            reject(new CtlHttpError(status, parsed));
            return;
          }
          resolve(parsed as T);
        });
      },
    );
    req.on("error", (e) => {
      // AbortSignal.timeout rejects with an AbortError whose message ("The
      // operation was aborted") names neither the host nor the timeout. Translate
      // it to the "timed out after Nms" phrasing so the surfaced message stays
      // truthful and callers/tests can key off it; the wrapper always names the
      // host either way.
      const msg = e.name === "AbortError" ? `timed out after ${timeoutMs}ms` : e.message;
      reject(new Error(`ctl: connection to ${host}:${config.port} failed: ${msg}`));
    });
    if (payload !== null) req.write(payload);
    req.end();
  });
}
