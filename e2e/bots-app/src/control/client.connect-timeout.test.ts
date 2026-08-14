import { EventEmitter } from "node:events";
import type { ClientRequestArgs } from "node:http";

import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * Deterministic, platform-independent regression lock for the connect-phase
 * timeout in {@link ctlRequest}.
 *
 * The bug: `req.setTimeout` is a socket-INACTIVITY timer that only arms AFTER
 * the TCP connection is established, so it never bounds a connect that never
 * completes (a node-partitioned pod; an unrouted host). The fix passes
 * `signal: AbortSignal.timeout(timeoutMs)` to `http.request`, which bounds the
 * WHOLE request including connect.
 *
 * A real hung connect can only be produced with root/network control
 * (iptables/netem) — on Linux CI, `127.0.0.2` refuses instantly, so the
 * existing `client.test.ts` tests do NOT fail on the un-fixed code there (the
 * blackhole branch is only ever exercised on macOS). This file closes that gap
 * by mocking the socket layer with a FAITHFUL hung-connect stand-in:
 *
 *   1. It honors `options.signal` exactly as a real `http.request` does — when
 *      the signal aborts, the request emits an `AbortError` on `error`. That is
 *      the only way the request settles, mirroring reality where `AbortSignal`
 *      is what bounds the connect phase.
 *   2. It does NOT invoke the `req.setTimeout` callback — faithful for the
 *      property under test: that inactivity timer does not fire at the requested
 *      `timeoutMs` during a hung connect (in real Node it trips only at the
 *      platform's multi-second connect boundary, never at `timeoutMs`), so
 *      modeling it as never-firing makes the un-fixed path hang deterministically
 *      rather than flakily at ~5s.
 *
 * Together these make the test mutation-sensitive: with the AbortSignal fix the
 * request rejects at `timeoutMs`; revert to `req.setTimeout` and nothing fires,
 * so the promise hangs until vitest kills it and the test FAILS.
 */

const mocks = vi.hoisted(() => ({
  request: vi.fn(),
}));

vi.mock("node:http", () => ({ request: mocks.request }));

// Imported after the mock is declared; vitest hoists `vi.mock` above all
// imports, so `client.ts`'s `import { request } from "node:http"` binds to the
// mock above.
import { ctlRequest } from "./client";

/** A ClientRequest stand-in for a socket whose TCP connect never completes. */
function hungConnectRequest(options: ClientRequestArgs): EventEmitter {
  const req = new EventEmitter();
  Object.assign(req, {
    // Inactivity timer — deliberately a no-op: in real Node it does not fire at
    // the requested `timeoutMs` during a hung connect (only at the platform's
    // ~multi-second connect boundary). Modeling it as never-firing is what makes
    // the OLD `req.setTimeout` fix hang here (deterministically, vs flakily ~5s).
    setTimeout: vi.fn(),
    write: vi.fn(),
    end: vi.fn(),
    destroy: vi.fn(),
  });
  const signal = options.signal;
  if (signal) {
    const onAbort = () => {
      // Real Node surfaces a timeout-driven request abort on the `error` event
      // as an AbortError (name "AbortError", code "ABORT_ERR").
      const err = Object.assign(new Error("The operation was aborted"), {
        name: "AbortError",
        code: "ABORT_ERR",
      });
      req.emit("error", err);
    };
    if (signal.aborted) queueMicrotask(onAbort);
    else signal.addEventListener("abort", onAbort, { once: true });
  }
  return req;
}

describe("ctlRequest connect-phase timeout (blackholed connect)", () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it("rejects at the configured timeoutMs instead of hanging on the SYN-retry budget", async () => {
    mocks.request.mockImplementation((options: ClientRequestArgs) => hungConnectRequest(options));

    const start = performance.now();
    await expect(
      ctlRequest({ host: "10.255.255.1", port: 65000, token: "t", timeoutMs: 40 }, "GET", "/bots"),
    ).rejects.toThrow(/timed out after 40ms/);
    // Bounded by `timeoutMs` (40ms), NOT the multi-second platform SYN-retry
    // budget the un-fixed `req.setTimeout` path would wait out.
    expect(performance.now() - start).toBeLessThan(2000);
  });

  it("names the target host:port so the conductor can log which pod hung", async () => {
    mocks.request.mockImplementation((options: ClientRequestArgs) => hungConnectRequest(options));

    await expect(
      ctlRequest(
        {
          host: "videocall-bots-3.videocall-bots.bot-load.svc",
          port: 8080,
          token: "t",
          timeoutMs: 40,
        },
        "GET",
        "/bots",
      ),
    ).rejects.toThrow(/videocall-bots-3\.videocall-bots\.bot-load\.svc:8080/);
  });
});
