import { request } from "node:http";

/**
 * Configuration for a `bots-app ctl` HTTP request. The control server
 * always binds to `127.0.0.1`, so the host is fixed; the client only
 * needs the port + token.
 */
export interface CtlClientConfig {
  port: number;
  token: string;
}

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
  return new Promise<T>((resolve, reject) => {
    const req = request(
      {
        host: "127.0.0.1",
        port: config.port,
        method,
        path,
        headers,
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
      reject(new Error(`ctl: connection to 127.0.0.1:${config.port} failed: ${e.message}`));
    });
    if (payload !== null) req.write(payload);
    req.end();
  });
}
