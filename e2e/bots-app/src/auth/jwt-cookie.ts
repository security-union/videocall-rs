import { BrowserContext } from "@playwright/test";

import { injectSessionCookie } from "../../../helpers/auth";

export interface JwtCookieAuthOptions {
  email: string;
  displayName: string;
  baseURL: string;
}

export async function applyJwtCookieAuth(
  context: BrowserContext,
  opts: JwtCookieAuthOptions,
): Promise<void> {
  await injectSessionCookie(context, {
    email: opts.email,
    name: opts.displayName,
    baseURL: opts.baseURL,
  });
}
