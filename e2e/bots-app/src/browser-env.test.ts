import { describe, expect, it } from "vitest";

import { browserEnvWithoutSecrets } from "./browser-env";

describe("browserEnvWithoutSecrets (#2035, #2914)", () => {
  it("strips BOT_EMAIL/BOT_PASSWORD, their ordinal-suffixed variants, and BOT_CTL_TOKEN", () => {
    const filtered = browserEnvWithoutSecrets({
      PATH: "/usr/bin",
      DISPLAY: ":0",
      BOT_EMAIL: "single@example.test",
      BOT_PASSWORD: "single-pw",
      BOT_EMAIL_0: "alice@example.test",
      BOT_PASSWORD_0: "pw0",
      BOT_EMAIL_19: "tina@example.test",
      BOT_PASSWORD_19: "pw19",
      BOT_CTL_TOKEN: "fleet-ctl-secret",
    });
    expect(filtered).toEqual({ PATH: "/usr/bin", DISPLAY: ":0" });
  });

  it("keeps non-credential vars, including unrelated BOT_* config and headed-display vars", () => {
    const filtered = browserEnvWithoutSecrets({
      PATH: "/usr/bin",
      DISPLAY: ":0",
      XAUTHORITY: "/home/op/.Xauthority",
      BOT_HW_CONCURRENCY: "6",
      BOT_AUTH: "form-login",
      BOT_CTL_PORT: "8080",
      BOT_CTL_BIND: "0.0.0.0",
      BOT_EMAIL_2: "carol@example.test",
    });
    expect(filtered).toEqual({
      PATH: "/usr/bin",
      DISPLAY: ":0",
      XAUTHORITY: "/home/op/.Xauthority",
      BOT_HW_CONCURRENCY: "6",
      BOT_AUTH: "form-login",
      BOT_CTL_PORT: "8080",
      BOT_CTL_BIND: "0.0.0.0",
    });
  });

  it("strips JWT_SECRET by exact name, keeping the JWT_SECRET_FILE near-miss", () => {
    const filtered = browserEnvWithoutSecrets({
      PATH: "/usr/bin",
      JWT_SECRET: "deployment-signing-key",
      JWT_SECRET_FILE: "/run/secrets/jwt",
    });
    expect(filtered).toEqual({ PATH: "/usr/bin", JWT_SECRET_FILE: "/run/secrets/jwt" });
  });

  it("drops undefined values", () => {
    const filtered = browserEnvWithoutSecrets({ PATH: "/usr/bin", MAYBE: undefined });
    expect(filtered).toEqual({ PATH: "/usr/bin" });
  });
});
