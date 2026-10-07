import { describe, expect, it, vi } from "vitest";

import {
  buildDecodeBudgetInitScript,
  DECODE_BUDGET_OFF_VALUE,
  DECODE_BUDGET_STORAGE_KEY,
  DecodeBudgetNotAppliedError,
  resolveDecodeBudget,
} from "./decode-budget";

const APP = "http://localhost:3001";

function runInitScript(origin: string, setItem: (k: string, v: string) => void) {
  const consoleError = vi.fn();
  new Function("location", "localStorage", "console", buildDecodeBudgetInitScript(APP))(
    { origin },
    { setItem },
    { error: consoleError },
  );
  return consoleError;
}

describe("resolveDecodeBudget", () => {
  it.each([
    [undefined, "client"],
    ["", "client"],
    ["client", "client"],
    [" Client ", "client"],
    ["off", "off"],
    ["OFF", "off"],
  ])("resolves %j to %s", (raw, value) => {
    expect(resolveDecodeBudget(raw)).toEqual({ kind: "ok", value });
  });

  it.each(["all", "auto", "on", "false", "0"])("rejects %j", (raw) => {
    expect(resolveDecodeBudget(raw)).toEqual({
      kind: "invalid",
      message: `--decode-budget must be "client" or "off", got "${raw}"`,
    });
  });
});

describe("buildDecodeBudgetInitScript", () => {
  it("seeds the override on the app origin", () => {
    const setItem = vi.fn();
    expect(runInitScript(APP, setItem)).not.toHaveBeenCalled();
    expect(setItem.mock.calls).toEqual([[DECODE_BUDGET_STORAGE_KEY, DECODE_BUDGET_OFF_VALUE]]);
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
    expect(String(consoleError.mock.calls[0][0])).toContain("decode-budget override failed");
  });
});

describe("DecodeBudgetNotAppliedError", () => {
  it("names the key, the value it read and the value it needed", () => {
    const err = new DecodeBudgetNotAppliedError(null);
    expect(err.observed).toBeNull();
    expect(err.message).toContain(`${DECODE_BUDGET_STORAGE_KEY} read back null`);
    expect(err.message).toContain(`not "${DECODE_BUDGET_OFF_VALUE}"`);
  });
});
