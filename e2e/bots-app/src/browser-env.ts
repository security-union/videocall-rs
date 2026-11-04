/**
 * Env-var names removed from every Chromium the bots-app launches: the fleet's
 * `BOT_EMAIL[_N]`/`BOT_PASSWORD[_N]`/`BOT_CTL_TOKEN[_N]` (#2035, #2072) and the
 * `--auth jwt` signing key `JWT_SECRET`. The browser never reads them.
 */
const BROWSER_ENV_STRIP_RE = /^(BOT_(EMAIL|PASSWORD|CTL_TOKEN)(_\d+)?|JWT_SECRET)$/;

/** For Playwright's launch `env` option, which replaces the browser env rather than merging. */
export function browserEnvWithoutSecrets(
  source: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(source)) {
    if (v !== undefined && !BROWSER_ENV_STRIP_RE.test(k)) out[k] = v;
  }
  return out;
}
