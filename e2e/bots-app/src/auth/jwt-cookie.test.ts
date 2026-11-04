import jwt from "jsonwebtoken";
import { describe, expect, it } from "vitest";

import { applyJwtCookieAuth } from "./jwt-cookie";

async function claims(notAfterSecs?: number): Promise<{ exp: number; iat: number }> {
  let value = "";
  const context = {
    clearCookies: async () => {},
    addCookies: async (cookies: { value: string }[]) => {
      value = cookies[0].value;
    },
  };
  await applyJwtCookieAuth(context as never, {
    email: "probe-00@bots-app.local",
    displayName: "probe-00",
    baseURL: "https://ui.test.invalid",
    notAfterSecs,
  });
  return jwt.decode(value) as { exp: number; iat: number };
}

describe("applyJwtCookieAuth session expiry", () => {
  it("expires the session cookie by the deadline", async () => {
    const deadline = Math.floor(Date.now() / 1000) + 835;
    expect((await claims(deadline)).exp).toBeLessThanOrEqual(deadline);
  });

  it("refuses to mint once the deadline has passed", async () => {
    await expect(claims(Math.floor(Date.now() / 1000))).rejects.toThrow("has passed");
  });

  it("keeps the shared helper's one-hour lifetime when no deadline is given", async () => {
    const c = await claims();
    expect(c.exp - c.iat).toBe(3600);
  });
});
