import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import {
  createServer,
  request as httpRequest,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http";
import { extname, join, resolve } from "node:path";
import { spawn, type ChildProcess } from "node:child_process";

import { findLatestTokenFile, readTokenFile, type TokenFileContents } from "./control/auth";

/**
 * Options accepted by {@link startDashboardServer}. Mirrors the CLI's
 * `bots-app dashboard` flag set so the same defaulting logic can run
 * unit-tested out-of-process.
 */
export interface DashboardOptions {
  /**
   * Port to bind the dashboard's Node-side HTTP listener to. `0`
   * means "let the kernel pick"; the caller is responsible for
   * reading the resolved port back off the returned handle.
   */
  port: number;
  /**
   * Ctl token + port. The dashboard injects this as
   * `Authorization: Bearer …` on every proxied `/api/*` request — the
   * browser never sees it.
   */
  ctl: { port: number; token: string };
  /**
   * Filesystem path to the dashboard `dist/` directory. When present,
   * the dashboard serves the static build. When absent, the caller
   * should arrange for a Vite dev process to serve from `index.html`
   * via the same proxy.
   */
  distDir?: string;
  /**
   * Asset directories surfaced to the launch form (`/api/assets/audio`
   * and `/api/assets/costumes`). The dashboard reads file names only —
   * never the contents.
   */
  assetsDir: string;
  /**
   * Hook fired once the server is listening. Used by the CLI to print
   * the listen URL and (optionally) open a browser.
   */
  onListen?: (info: { port: number }) => void;
}

export interface DashboardServerHandle {
  port: number;
  server: Server;
  close(): Promise<void>;
}

/**
 * Spin up the dashboard's Node-side HTTP listener. Binds to
 * `127.0.0.1` only — never exposed over the network. Three classes
 * of request:
 *
 *   1. `/api/healthz` and `/api/daemon` — small JSON endpoints
 *      synthesized locally (the latter exposes the discovered ctl
 *      port + pid for the layout's status badge).
 *   2. `/api/assets/{audio,costumes}` — directory listings under the
 *      caller-supplied `assetsDir`. Filenames only, no contents.
 *   3. `/api/*` everything else — proxied to the ctl API on
 *      `127.0.0.1:<ctl.port>` with the bearer token attached
 *      server-side.
 *
 * Static-file serving (when `distDir` is set) is intentionally
 * minimal — no directory listing, no symlink resolution outside
 * `distDir`, MIME types covering only what Vite emits.
 */
export function startDashboardServer(opts: DashboardOptions): Promise<DashboardServerHandle> {
  return new Promise<DashboardServerHandle>((resolveFn, reject) => {
    const server = createServer((req, res) => {
      handleRequest(req, res, opts).catch((err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        sendJson(res, 500, { error: `dashboard internal error: ${msg}` });
      });
    });
    server.once("error", reject);
    server.listen(opts.port, "127.0.0.1", () => {
      server.off("error", reject);
      const addr = server.address();
      if (addr === null || typeof addr === "string") {
        reject(new Error("dashboard: unexpected address type from listen()"));
        return;
      }
      if (opts.onListen) opts.onListen({ port: addr.port });
      resolveFn({
        port: addr.port,
        server,
        close: () =>
          new Promise<void>((res, rej) => {
            server.close((e) => (e ? rej(e) : res()));
          }),
      });
    });
  });
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
  opts: DashboardOptions,
): Promise<void> {
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const { pathname } = url;
  const method = req.method ?? "GET";

  // 1. Locally-synthesized endpoints. These never reach the ctl API.
  if (method === "GET" && pathname === "/api/daemon") {
    sendJson(res, 200, {
      port: opts.ctl.port,
      pid: process.pid,
      startedAt: new Date().toISOString(),
    });
    return;
  }
  if (method === "GET" && pathname === "/api/assets/audio") {
    sendJson(res, 200, { files: listAssetFiles(join(opts.assetsDir, "audio"), [".wav"]) });
    return;
  }
  if (method === "GET" && pathname === "/api/assets/costumes") {
    sendJson(res, 200, { files: listAssetFiles(join(opts.assetsDir, "costumes"), [".y4m"]) });
    return;
  }

  // 2. Proxy everything else under /api/* to the ctl API.
  if (pathname.startsWith("/api/")) {
    await proxyToCtl(req, res, opts, pathname);
    return;
  }

  // 3. Static serve from dist/ (when built).
  if (opts.distDir && existsSync(opts.distDir)) {
    serveStatic(res, opts.distDir, pathname);
    return;
  }

  // No built dist, nothing else matched — fall through to a tiny
  // bootstrap page that nudges the operator to run `npm run dev` in
  // the dashboard subtree.
  sendHtml(
    res,
    200,
    `<!doctype html><meta charset="utf-8"><title>bots-app dashboard</title><body style="font-family:system-ui;padding:2rem">
      <h1>bots-app dashboard</h1>
      <p>No built UI found at <code>${opts.distDir ?? "(unset)"}</code>.</p>
      <p>Either run <code>npm run build</code> in <code>e2e/bots-app/dashboard/</code>,
      or start the Vite dev server from that directory while leaving
      this Node sidecar running.</p>
    </body>`,
  );
}

function proxyToCtl(
  req: IncomingMessage,
  res: ServerResponse,
  opts: DashboardOptions,
  pathname: string,
): Promise<void> {
  // Strip the `/api/` prefix; everything after is the ctl path.
  // `/api/launch` → `/launch`; `/api/bots/<id>/mute` → `/bots/<id>/mute`.
  const ctlPath = pathname.replace(/^\/api/, "") || "/";
  const search = req.url?.includes("?") ? req.url.slice(req.url.indexOf("?")) : "";
  const headers: Record<string, string | string[]> = {
    accept: "application/json",
    authorization: `Bearer ${opts.ctl.token}`,
  };
  // Pass through content-type + content-length for POST bodies. We
  // don't buffer the body; we pipe the original request straight into
  // the proxied request so large payloads (we have none today) would
  // flow without copying.
  if (req.headers["content-type"]) headers["content-type"] = req.headers["content-type"]!;
  if (req.headers["content-length"]) headers["content-length"] = req.headers["content-length"]!;

  return new Promise<void>((resolveFn) => {
    const upstream = httpRequest(
      {
        host: "127.0.0.1",
        port: opts.ctl.port,
        method: req.method ?? "GET",
        path: ctlPath + search,
        headers,
      },
      (upstreamRes) => {
        // Mirror upstream status + content-type only; we never want
        // upstream's `connection: close` or transfer-encoding leaking
        // into the browser.
        const passHeaders: Record<string, string> = {};
        if (upstreamRes.headers["content-type"]) {
          passHeaders["content-type"] = upstreamRes.headers["content-type"];
        }
        res.writeHead(upstreamRes.statusCode ?? 502, passHeaders);
        upstreamRes.pipe(res);
        upstreamRes.on("end", () => resolveFn());
      },
    );
    upstream.on("error", (err) => {
      sendJson(res, 502, {
        error: `ctl proxy failed: ${err.message}`,
        ctl: { host: "127.0.0.1", port: opts.ctl.port },
      });
      resolveFn();
    });
    req.pipe(upstream);
  });
}

/**
 * Read `dir` and return the basenames of all files whose extension is
 * in the supplied `allowed` list. Errors (missing dir, permission)
 * become an empty list — the dashboard surfaces this as "no costumes
 * found" rather than a 500.
 *
 * Names only — never paths. The dashboard's CLI form passes the
 * basename back as a hint; the orchestrator re-resolves the absolute
 * path under its own assetsDir before launching.
 */
export function listAssetFiles(dir: string, allowed: readonly string[]): string[] {
  try {
    const entries = readdirSync(dir);
    return entries
      .filter((name) => {
        if (name.startsWith(".") || name.startsWith("_")) return false;
        const ext = extname(name).toLowerCase();
        return allowed.includes(ext);
      })
      .sort();
  } catch {
    return [];
  }
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".mjs": "application/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
  ".map": "application/json; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
};

function serveStatic(res: ServerResponse, distDir: string, pathname: string): void {
  // Normalize then ensure the resolved path is inside `distDir`. Block
  // any kind of `../` traversal up front.
  const requested = pathname === "/" ? "/index.html" : pathname;
  const fsPath = resolve(distDir, "." + requested);
  if (!fsPath.startsWith(resolve(distDir))) {
    sendJson(res, 403, { error: "forbidden" });
    return;
  }
  if (!existsSync(fsPath)) {
    // SPA fallback — Vite-built apps expect any unknown route to
    // hand back index.html so React Router (or our state-based
    // routing) can handle it.
    const fallback = resolve(distDir, "index.html");
    if (existsSync(fallback)) {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(readFileSync(fallback));
      return;
    }
    sendJson(res, 404, { error: "not found" });
    return;
  }
  const st = statSync(fsPath);
  if (st.isDirectory()) {
    sendJson(res, 403, { error: "directory listing forbidden" });
    return;
  }
  const ct = MIME[extname(fsPath).toLowerCase()] ?? "application/octet-stream";
  res.writeHead(200, { "content-type": ct, "content-length": st.size });
  res.end(readFileSync(fsPath));
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

function sendHtml(res: ServerResponse, status: number, html: string): void {
  res.writeHead(status, {
    "content-type": "text/html; charset=utf-8",
    "content-length": Buffer.byteLength(html),
  });
  res.end(html);
}

/**
 * Resolve a {@link TokenFileContents} from the union of CLI flags.
 * Priority matches the `ctl` subcommands:
 *   1. `--ctl-port` + `--ctl-token` (both required)
 *   2. `--ctl-token-file <path>`
 *   3. Auto-discover the most-recent `ctl-*.token` under `runDir`.
 *
 * Throws on bad combinations. Exposed for unit tests; the CLI also
 * calls it directly.
 */
export async function resolveCtlConfig(args: {
  port?: number;
  token?: string;
  tokenFile?: string;
  runDir: string;
}): Promise<TokenFileContents> {
  if (args.port !== undefined || args.token !== undefined) {
    if (args.port === undefined || args.token === undefined) {
      throw new Error("dashboard: --ctl-port and --ctl-token must be supplied together");
    }
    return { port: args.port, token: args.token, startedAt: new Date().toISOString(), pid: 0 };
  }
  let tokenFilePath = args.tokenFile ?? null;
  if (tokenFilePath === null) {
    tokenFilePath = await findLatestTokenFile(args.runDir);
    if (tokenFilePath === null) {
      throw new Error(
        `dashboard: no ctl token file found under ${args.runDir}. ` +
          "Start an orchestrator with `bots-app run --ctl-port auto`, " +
          "or pass --ctl-token-file / --ctl-port + --ctl-token explicitly.",
      );
    }
  }
  return readTokenFile(tokenFilePath);
}

/**
 * Spawn `npm run dev` inside the dashboard subtree, forwarding the
 * backend port so Vite's proxy targets the right Node sidecar. The
 * returned `ChildProcess` is killed when the parent process exits.
 *
 * In built mode (dist/ exists), this is NOT called — the parent
 * serves the static files directly and Vite is not involved.
 */
export function spawnViteDev(args: { dashboardDir: string; backendPort: number }): ChildProcess {
  const child = spawn("npm", ["run", "dev"], {
    cwd: args.dashboardDir,
    stdio: ["ignore", "inherit", "inherit"],
    env: { ...process.env, DASHBOARD_BACKEND_PORT: String(args.backendPort) },
  });
  const killChild = (): void => {
    if (!child.killed) child.kill("SIGTERM");
  };
  process.once("exit", killChild);
  process.once("SIGINT", killChild);
  process.once("SIGTERM", killChild);
  return child;
}
