import { describe, expect, it, vi } from "vitest";

import {
  buildSpeakingGlowInitScript,
  resolveSpeakingGlow,
  SPEAKING_GLOW_DEFAULT,
  SPEAKING_GLOW_OFF_VALUE,
  SPEAKING_GLOW_STORAGE_KEY,
  SpeakingGlowNotAppliedError,
  speakingGlowVerdict,
} from "./speaking-glow";

const APP = "http://localhost:3001";

function runInitScript(origin: string, setItem: (k: string, v: string) => void) {
  const consoleError = vi.fn();
  new Function("location", "localStorage", "console", buildSpeakingGlowInitScript(APP))(
    { origin },
    { setItem },
    { error: consoleError },
  );
  return consoleError;
}

describe("resolveSpeakingGlow", () => {
  it.each([
    [undefined, "client"],
    ["", "client"],
    ["client", "client"],
    [" Client ", "client"],
    ["off", "off"],
    ["OFF", "off"],
  ])("resolves %j to %s", (raw, value) => {
    expect(resolveSpeakingGlow(raw)).toEqual({ kind: "ok", value });
  });

  it("keeps the client's glow by default", () => {
    expect(SPEAKING_GLOW_DEFAULT).toBe("client");
  });

  it.each(["on", "false", "0", "none"])("rejects %j", (raw) => {
    expect(resolveSpeakingGlow(raw)).toEqual({
      kind: "invalid",
      message: `--speaking-glow (or BOT_SPEAKING_GLOW) must be "client" or "off", got "${raw}"`,
    });
  });
});

describe("buildSpeakingGlowInitScript", () => {
  it("seeds the preference on the app origin", () => {
    const setItem = vi.fn();
    expect(runInitScript(APP, setItem)).not.toHaveBeenCalled();
    expect(setItem.mock.calls).toEqual([[SPEAKING_GLOW_STORAGE_KEY, SPEAKING_GLOW_OFF_VALUE]]);
  });

  it("leaves another origin's storage alone (the form-login identity provider)", () => {
    const setItem = vi.fn();
    runInitScript("https://id.example.test", setItem);
    expect(setItem).not.toHaveBeenCalled();
  });

  it("logs instead of throwing when storage refuses the write", () => {
    const consoleError = runInitScript(APP, () => {
      throw new Error("QuotaExceededError");
    });
    expect(consoleError).toHaveBeenCalledTimes(1);
    expect(String(consoleError.mock.calls[0][0])).toContain("speaking-glow preference failed");
  });
});

describe("speakingGlowVerdict", () => {
  it("runs an off bot only when the preference read back", () => {
    expect(speakingGlowVerdict("off", "false")).toBe("ok");
  });

  it.each([null, "true", "False", ""])("fails an off bot that read back %j", (readback) => {
    expect(speakingGlowVerdict("off", readback)).toBe("not-applied");
  });

  it.each([null, "true", "anything"])("runs a client bot that read back %j", (readback) => {
    expect(speakingGlowVerdict("client", readback)).toBe("ok");
  });

  it("warns when a client bot's replayed storage turned the glow off", () => {
    expect(speakingGlowVerdict("client", "false")).toBe("inherited-off");
  });
});

describe("SpeakingGlowNotAppliedError", () => {
  it("names the key, the value it read and the value it needed", () => {
    const err = new SpeakingGlowNotAppliedError(null);
    expect(err.observed).toBeNull();
    expect(err.message).toContain(`${SPEAKING_GLOW_STORAGE_KEY} read back null`);
    expect(err.message).toContain(`not "${SPEAKING_GLOW_OFF_VALUE}"`);
  });
});
