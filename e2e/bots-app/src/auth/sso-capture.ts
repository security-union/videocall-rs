import { mkdirSync } from "node:fs";
import { dirname } from "node:path";

import { chromium, type Browser, type BrowserContext } from "@playwright/test";

/**
 * Default URL the operator is navigated to when starting an SSO
 * recapture. Production HCL deployments live behind `app.videocall.fnxlabs.com`
 * — landing there triggers the HCL SSO redirect, which is exactly the
 * portal the operator needs to authenticate against in order to seed a
 * valid `hcl-sso.json` storage state for the bots.
 */
export const DEFAULT_SSO_START_URL = "https://app.videocall.fnxlabs.com/";

/**
 * Handle returned by {@link openSsoCaptureBrowser}. Holds the live
 * Playwright objects so a caller can later call `storageState({ path })`
 * once the operator has finished logging in.
 *
 * Closing semantics: callers MUST eventually call `close()` (which tears
 * down the context and the underlying browser) — whether they saved the
 * state or aborted. The control-server endpoints track this map and
 * also wire their teardown into the process SIGINT/SIGTERM handler so a
 * stranded browser does not leak on shutdown.
 */
export interface SsoCaptureSession {
  browser: Browser;
  context: BrowserContext;
  /**
   * Save the captured cookies + localStorage to `path` and tear the
   * browser down. Idempotent: a second call is a no-op.
   */
  saveAndClose(path: string): Promise<void>;
  /**
   * Tear the browser down WITHOUT saving. Idempotent.
   */
  close(): Promise<void>;
}

/**
 * Spin up a headed Chromium, open `startUrl`, and return a handle the
 * caller can keep alive until the operator finishes the SSO login. The
 * caller is responsible for capturing the state (`saveAndClose`) OR
 * cancelling (`close`) — neither happens automatically.
 *
 * Extracted from the legacy `bots-app sso-login` CLI flow so the
 * dashboard's `POST /api/sso/recapture` and the CLI subcommand share
 * one code path. The CLI flow adds an `await rl.question(...)` prompt
 * around this handle to wait for the operator at the terminal; the
 * dashboard flow keeps the handle in a server-side map keyed by a
 * recapture-session id, and the operator clicks a button in the UI to
 * trigger the save.
 */
export async function openSsoCaptureBrowser(opts: {
  startUrl: string;
}): Promise<SsoCaptureSession> {
  const browser = await chromium.launch({ headless: false });
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  await page.goto(opts.startUrl, { waitUntil: "domcontentloaded" });

  let closed = false;
  const tearDown = async (): Promise<void> => {
    if (closed) return;
    closed = true;
    try {
      await context.close();
    } catch {
      // The operator may have closed the window already — swallow.
    }
    try {
      await browser.close();
    } catch {
      // ditto.
    }
  };

  return {
    browser,
    context,
    saveAndClose: async (path: string): Promise<void> => {
      if (closed) {
        throw new Error("SSO capture session has already been closed");
      }
      mkdirSync(dirname(path), { recursive: true });
      await context.storageState({ path });
      await tearDown();
    },
    close: tearDown,
  };
}

/**
 * High-level convenience used by the `bots-app sso-login` CLI
 * subcommand. Opens the headed browser, awaits a caller-supplied
 * `waitForOperator` promise (typically a `readline.question(...)`),
 * then saves and tears down.
 *
 * Returning the resolved output path makes the CLI's logging
 * straightforward and keeps the call site short.
 */
export async function captureSsoStateInteractive(opts: {
  startUrl: string;
  outPath: string;
  waitForOperator: () => Promise<void>;
}): Promise<string> {
  const session = await openSsoCaptureBrowser({ startUrl: opts.startUrl });
  try {
    await opts.waitForOperator();
  } catch (err) {
    await session.close();
    throw err;
  }
  await session.saveAndClose(opts.outPath);
  return opts.outPath;
}
