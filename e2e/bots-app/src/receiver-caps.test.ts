import vm from "node:vm";

import { describe, expect, it } from "vitest";

import {
  buildReceiverConfigInitScript,
  buildReceiverConfigOverrides,
  MAX_RECEIVED_LAYER_CEILING,
  resolveMaxReceivedLayer,
  resolveSkipCanvasPaint,
} from "./receiver-caps";

describe("resolveMaxReceivedLayer (#2068)", () => {
  it("returns undefined (no cap) when unset, empty, or whitespace", () => {
    expect(resolveMaxReceivedLayer(undefined)).toEqual({ kind: "ok", value: undefined });
    expect(resolveMaxReceivedLayer("")).toEqual({ kind: "ok", value: undefined });
    expect(resolveMaxReceivedLayer("   ")).toEqual({ kind: "ok", value: undefined });
  });

  it("accepts 0 as a valid cap (base rung only — the primary #2068 use)", () => {
    // Unlike hardware-concurrency, 0 is NOT a disable sentinel here.
    expect(resolveMaxReceivedLayer("0")).toEqual({ kind: "ok", value: 0 });
    expect(resolveMaxReceivedLayer(" 0 ")).toEqual({ kind: "ok", value: 0 });
  });

  it("accepts higher non-negative integers", () => {
    expect(resolveMaxReceivedLayer("1")).toEqual({ kind: "ok", value: 1 });
    expect(resolveMaxReceivedLayer("2")).toEqual({ kind: "ok", value: 2 });
  });

  it("rejects negative, fractional, exponent, and prefix-numeric junk", () => {
    expect(resolveMaxReceivedLayer("-1").kind).toBe("invalid");
    expect(resolveMaxReceivedLayer("1.5").kind).toBe("invalid");
    expect(resolveMaxReceivedLayer("1e2").kind).toBe("invalid");
    // Number.parseInt("2junk") would silently return 2 — must be rejected.
    expect(resolveMaxReceivedLayer("2junk").kind).toBe("invalid");
    expect(resolveMaxReceivedLayer("abc").kind).toBe("invalid");
  });

  it("rejects values above the ceiling (would break the client's whole __APP_CONFIG parse)", () => {
    // At/below the ceiling is accepted; above it fails fast at the CLI rather
    // than overflowing the client's u32 and erroring the whole config parse.
    expect(resolveMaxReceivedLayer(String(MAX_RECEIVED_LAYER_CEILING))).toEqual({
      kind: "ok",
      value: MAX_RECEIVED_LAYER_CEILING,
    });
    expect(resolveMaxReceivedLayer(String(MAX_RECEIVED_LAYER_CEILING + 1)).kind).toBe("invalid");
    expect(resolveMaxReceivedLayer("99999999999").kind).toBe("invalid");
  });
});

describe("resolveSkipCanvasPaint (#2069)", () => {
  it("returns undefined (inherit deployment) when unset/empty", () => {
    expect(resolveSkipCanvasPaint(undefined)).toEqual({ kind: "ok", value: undefined });
    expect(resolveSkipCanvasPaint("")).toEqual({ kind: "ok", value: undefined });
    expect(resolveSkipCanvasPaint("  ")).toEqual({ kind: "ok", value: undefined });
  });

  it("accepts truthy tokens (case-insensitive) → true", () => {
    for (const t of ["true", "TRUE", "True", "1", "yes", "on", " on "]) {
      expect(resolveSkipCanvasPaint(t)).toEqual({ kind: "ok", value: true });
    }
  });

  it("accepts falsy tokens (case-insensitive) → false", () => {
    for (const t of ["false", "FALSE", "0", "no", "off"]) {
      expect(resolveSkipCanvasPaint(t)).toEqual({ kind: "ok", value: false });
    }
  });

  it("rejects unrecognized values", () => {
    expect(resolveSkipCanvasPaint("maybe").kind).toBe("invalid");
    expect(resolveSkipCanvasPaint("2").kind).toBe("invalid");
  });
});

describe("buildReceiverConfigOverrides", () => {
  it("returns null when nothing is set (⇒ inject nothing, default behavior)", () => {
    expect(buildReceiverConfigOverrides({})).toBeNull();
    expect(buildReceiverConfigOverrides({ maxReceivedLayer: undefined })).toBeNull();
  });

  it("includes only the set keys", () => {
    expect(buildReceiverConfigOverrides({ maxReceivedLayer: 0 })).toEqual({ maxReceivedLayer: 0 });
    expect(buildReceiverConfigOverrides({ maxReceivedLayer: 2 })).toEqual({ maxReceivedLayer: 2 });
  });

  it("serializes skipCanvasPaint as a STRING (the client RuntimeConfig field is String)", () => {
    // A JS boolean would fail serde deserialization into `String`.
    expect(buildReceiverConfigOverrides({ skipCanvasPaint: true })).toEqual({
      skipCanvasPaint: "true",
    });
    expect(buildReceiverConfigOverrides({ skipCanvasPaint: false })).toEqual({
      skipCanvasPaint: "false",
    });
    const both = buildReceiverConfigOverrides({ maxReceivedLayer: 0, skipCanvasPaint: true });
    expect(both).toEqual({ maxReceivedLayer: 0, skipCanvasPaint: "true" });
    expect(typeof both?.skipCanvasPaint).toBe("string");
  });
});

// Evaluate a generated init-script the way Playwright's addInitScript runs it in
// the page: a document context whose global aliases itself as `window`. We run
// the script to install the __APP_CONFIG accessor, then drive further
// evaluations in the SAME context so `window.__APP_CONFIG = …` hits the
// installed setter — exactly as config.js does in the browser.
function afterConfigJs(
  overrides: Record<string, unknown>,
  configJsAssignExpr: string | null,
): { cfg: Record<string, unknown> | undefined; isFrozen: boolean } {
  const script = buildReceiverConfigInitScript(overrides);
  const sandbox: Record<string, unknown> = { console };
  sandbox.window = sandbox; // window === global, as in a browser document
  vm.createContext(sandbox);
  vm.runInContext(script, sandbox);
  if (configJsAssignExpr !== null) {
    vm.runInContext(`window.__APP_CONFIG = ${configJsAssignExpr};`, sandbox);
  }
  const json = vm.runInContext(
    "window.__APP_CONFIG === undefined ? 'undefined' : JSON.stringify(window.__APP_CONFIG)",
    sandbox,
  ) as string;
  const isFrozen = vm.runInContext(
    "window.__APP_CONFIG !== undefined && Object.isFrozen(window.__APP_CONFIG)",
    sandbox,
  ) as boolean;
  return { cfg: json === "undefined" ? undefined : JSON.parse(json), isFrozen };
}

describe("buildReceiverConfigInitScript (setter-merge injection seam)", () => {
  it("merges overrides OVER a frozen config.js assignment, preserving deployment keys", () => {
    // Production config.js does: window.__APP_CONFIG = Object.freeze({...})
    const { cfg, isFrozen } = afterConfigJs(
      { maxReceivedLayer: 0, skipCanvasPaint: "true" },
      'Object.freeze({ apiBaseUrl: "x", skipCanvasPaint: "false", experimentalSimulcastMaxLayers: 1 })',
    );
    // Deployment keys survive the merge.
    expect(cfg?.apiBaseUrl).toBe("x");
    expect(cfg?.experimentalSimulcastMaxLayers).toBe(1);
    // Overrides are injected...
    expect(cfg?.maxReceivedLayer).toBe(0);
    // ...and WIN over the deployment's own value (merge direction: overrides last).
    expect(cfg?.skipCanvasPaint).toBe("true");
    // The wasm reader sees a fresh, unfrozen merged object.
    expect(isFrozen).toBe(false);
  });

  it("returns the overrides even before config.js runs (seeded default, never undefined)", () => {
    const { cfg } = afterConfigJs({ maxReceivedLayer: 0 }, null);
    expect(cfg).toEqual({ maxReceivedLayer: 0 });
  });

  it("re-merges on a SECOND assignment (cannot be clobbered)", () => {
    const script = buildReceiverConfigInitScript({ skipCanvasPaint: "true" });
    const sandbox: Record<string, unknown> = { console };
    sandbox.window = sandbox;
    vm.createContext(sandbox);
    vm.runInContext(script, sandbox);
    vm.runInContext('window.__APP_CONFIG = { skipCanvasPaint: "false", a: 1 };', sandbox);
    vm.runInContext('window.__APP_CONFIG = { skipCanvasPaint: "false", b: 2 };', sandbox);
    const cfg = JSON.parse(
      vm.runInContext("JSON.stringify(window.__APP_CONFIG)", sandbox) as string,
    );
    expect(cfg.skipCanvasPaint).toBe("true"); // override still wins after re-assign
    expect(cfg.b).toBe(2); // last assignment's base is used
    expect(cfg.a).toBeUndefined();
  });
});
