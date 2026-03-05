import jwt from "jsonwebtoken";
import { BrowserContext } from "@playwright/test";

const JWT_SECRET = process.env.JWT_SECRET || "dev-jwt-secret-change-me";
const COOKIE_NAME = process.env.COOKIE_NAME || "session";

export function generateSessionToken(email: string, name: string, ttlSecs: number = 3600): string {
  const now = Math.floor(Date.now() / 1000);
  return jwt.sign(
    {
      sub: email,
      name: name,
      exp: now + ttlSecs,
      iat: now,
      iss: "videocall-meeting-backend",
    },
    JWT_SECRET,
    { algorithm: "HS256" },
  );
}

interface SessionCookieOptions {
  email?: string;
  name?: string;
  baseURL?: string;
}

export async function injectSessionCookie(
  context: BrowserContext,
  opts: SessionCookieOptions = {},
): Promise<void> {
  const email = opts.email || "e2e-test@videocall.rs";
  const name = opts.name || "E2ETestUser";
  const resolvedURL = opts.baseURL || "http://localhost:80";

  const token = generateSessionToken(email, name);
  const url = new URL(resolvedURL);

  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
}
