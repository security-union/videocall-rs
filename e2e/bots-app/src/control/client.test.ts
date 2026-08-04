import { createServer, type Server } from "node:http";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { NETSIM_PRESETS } from "../meeting-config";
import { generateToken } from "./auth";
import { CtlHttpError, ctlRequest } from "./client";
import { type ControlServerHandle, startControlServer } from "./server";
import { type BotRegistryEntry } from "./registry";

/**
 * Spec-required: "The `network` validation client-side rejects
 * unknown profiles before hitting the server." We validate via
 * `NETSIM_PRESETS.includes` in the ctl-command action. This test
 * is a contract assertion against `NETSIM_PRESETS` — guard rail for
 * accidental drift.
 */
describe("client-side network validation", () => {
  it("NETSIM_PRESETS rejects unknown profile names", () => {
    expect(NETSIM_PRESETS.includes("lossy_mobile")).toBe(true);
    expect(NETSIM_PRESETS.includes("bogus")).toBe(false);
  });
});

describe("ctlRequest", () => {
  let handle: ControlServerHandle;
  let token: string;

  beforeEach(async () => {
    token = generateToken();
    const registry = new Map<string, BotRegistryEntry>();
    handle = await startControlServer({
      port: 0,
      token,
      surface: {
        getRegistry: () => registry,
        triggerLeave: async () => {},
        forceKill: async () => {},
        applyTtl: () => {},
        changeNetwork: async () => {},
        setMicMuted: async () => {},
        setCameraOff: async () => {},
        setScreenShare: async () => {},
        duplicateBot: async () => "new-id",
        launchOne: async () => "new-id",
      },
    });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("issues a GET with the bearer token and parses the JSON response", async () => {
    const res = await ctlRequest<{ bots: unknown[] }>({ port: handle.port, token }, "GET", "/bots");
    expect(res.bots).toEqual([]);
  });

  it("surfaces non-2xx as CtlHttpError with the server's body", async () => {
    await expect(
      ctlRequest({ port: handle.port, token: "wrong" }, "GET", "/bots"),
    ).rejects.toBeInstanceOf(CtlHttpError);
  });

  it("defaults to 127.0.0.1 when host is omitted (back-compat)", async () => {
    const res = await ctlRequest<{ bots: unknown[] }>({ port: handle.port, token }, "GET", "/bots");
    expect(res.bots).toEqual([]);
  });

  it("honors an explicit host that matches the bound address", async () => {
    const res = await ctlRequest<{ bots: unknown[] }>(
      { host: "127.0.0.1", port: handle.port, token },
      "GET",
      "/bots",
    );
    expect(res.bots).toEqual([]);
  });

  it("actually connects to the configured host (not a hard-coded loopback)", async () => {
    // The server is bound to 127.0.0.1 ONLY. Targeting 127.0.0.2 must
    // fail to reach it — proving the client uses `config.host`. If host
    // were ignored and 127.0.0.1 substituted, this request would
    // succeed with a 200 and the assertion would fail.
    //
    // Portability: on Linux 127.0.0.2 is on `lo` and refuses immediately
    // (ECONNREFUSED); on macOS it is NOT assigned to lo0 and the connect
    // blackholes with no `error` event. `ctlRequest` bounds the whole request —
    // including the connect phase — with AbortSignal.timeout, so the macOS
    // blackhole path REJECTS at the configured `timeoutMs` (~500ms here) instead
    // of after the platform's multi-second SYN-retry budget. Both branches reject
    // well within vitest's default 5s, and the wrapper always names the host.
    await expect(
      ctlRequest({ host: "127.0.0.2", port: handle.port, token, timeoutMs: 500 }, "GET", "/bots"),
    ).rejects.toThrow(/127\.0\.0\.2/);
  });

  it("rejects via timeout when the server accepts but never responds", async () => {
    // Platform-independent lock for the ctlRequest timeout (the conductor's
    // resolve-retry only re-fires on a REJECTION, so a hung call must reject).
    // A server that accepts the connection and never replies makes an
    // un-timed-out request hang forever — vitest would kill it. With the
    // timeout, it rejects promptly with a message naming the host.
    const hung: Server = createServer(() => {
      /* intentionally never write a response */
    });
    await new Promise<void>((r) => hung.listen(0, "127.0.0.1", r));
    const port = (hung.address() as { port: number }).port;
    try {
      await expect(
        ctlRequest({ host: "127.0.0.1", port, token, timeoutMs: 300 }, "GET", "/bots"),
      ).rejects.toThrow(/timed out/);
    } finally {
      await new Promise<void>((r) => hung.close(() => r()));
    }
  });

  it("issues a POST with a JSON body when supplied", async () => {
    // /healthz is fine for the POST contract test even though the
    // server only routes GET on it — we just want to confirm the
    // request gets framed with content-type + content-length.
    // The server returns a 404 for POST /healthz, which surfaces as
    // a CtlHttpError; the test passes because that's still
    // round-trip evidence the request body was sent.
    await expect(
      ctlRequest({ port: handle.port, token }, "POST", "/healthz", {
        foo: "bar",
      }),
    ).rejects.toBeInstanceOf(CtlHttpError);
  });
});
