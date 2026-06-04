/**
 * Filter for Dioxus dev-server console noise that the bot picks up when
 * the target is served via `trunk serve` (the default in `make e2e-up`).
 *
 * Dioxus 0.7's dev tooling opens a `_dioxus?build_id=…` WebSocket for
 * hot-reload and sometimes serves the SPA `index.html` where the browser
 * expected JS during build-id resolution, producing a noisy
 * `Unexpected token '<'` syntax error. None of this is related to the
 * videocall stack and the per-launch volume is large enough to drown the
 * actually-interesting log lines.
 *
 * The check is intentionally narrow so it can't accidentally suppress
 * real diagnostics from production traces:
 *   - `Unexpected token '<'` is only treated as noise when the page is
 *     currently on the trunk dev server (host = `localhost:3001`). If a
 *     real SSO portal or CDN starts injecting HTML where JS is expected,
 *     we still want to see it.
 *   - `_dioxus?build_id=` matches both the WebSocket-failed message and
 *     any console.error mentioning the dev HMR socket URL.
 */
export interface NoiseContext {
  /** Current top-frame URL of the page, as reported by `page.url()`. */
  pageUrl: string;
}

/**
 * Hosts where Dioxus dev-server noise is expected. Kept narrow on
 * purpose — adding more here means more chances to swallow a real
 * production-side error.
 */
const DEV_SERVER_HOSTS = new Set<string>(["localhost:3001", "127.0.0.1:3001"]);

function isDevServerHost(pageUrl: string): boolean {
  try {
    const u = new URL(pageUrl);
    return DEV_SERVER_HOSTS.has(u.host);
  } catch {
    return false;
  }
}

/**
 * Returns `true` when the given error / console message originates from
 * the Dioxus dev server's hot-reload tooling and not from videocall
 * code. The caller is expected to swallow these and surface a single
 * one-line summary at the start of the run.
 *
 * `text` is the raw `Error.message` (for `pageerror`) or `ConsoleMessage.text()`
 * (for `console.error`).
 */
export function isDevServerNoise(text: string, ctx: NoiseContext): boolean {
  // The "Unexpected token '<'" syntax error fires when the dev server
  // returns `index.html` from a JS request — only treat it as noise
  // when we're actually pointed at the trunk dev server. Match exact
  // text (not substring) so a real "Unexpected token '<' at line N" in
  // production code is still surfaced.
  if (text === "Unexpected token '<'" && isDevServerHost(ctx.pageUrl)) {
    return true;
  }
  // Dioxus 0.7 HMR socket — its URL is stable across builds.
  if (text.includes("_dioxus?build_id=")) {
    return true;
  }
  return false;
}
