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

export async function injectSessionCookie(
  context: BrowserContext,
  email: string = "e2e-test@videocall.rs",
  name: string = "E2ETestUser",
): Promise<void> {
  const token = generateSessionToken(email, name);
  const baseURL = process.env.UI_URL || "http://localhost:80";
  const url = new URL(baseURL);

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
