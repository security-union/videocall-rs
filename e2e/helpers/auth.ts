import jwt from "jsonwebtoken";
import { BrowserContext } from "@playwright/test";

const JWT_SECRET = process.env.JWT_SECRET || "dev-jwt-secret-change-me";
const COOKIE_NAME = process.env.COOKIE_NAME || "session";

export function generateSessionToken(userId: string, name: string, ttlSecs: number = 3600): string {
  const now = Math.floor(Date.now() / 1000);
  return jwt.sign(
    {
      sub: userId,
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
  userId?: string;
  name?: string;
  baseURL?: string;
}

export async function injectSessionCookie(
  context: BrowserContext,
  opts: SessionCookieOptions = {},
): Promise<void> {
  const userId = opts.userId || "00000000-0000-4000-8000-000000000001";
  const name = opts.name || "E2ETestUser";
  const resolvedURL = opts.baseURL || "http://localhost:80";

  const token = generateSessionToken(userId, name);
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
