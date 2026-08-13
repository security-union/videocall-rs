import { createServer, type Server, type ServerResponse } from "node:http";

import { afterEach, describe, expect, it } from "vitest";

import {
  CTL_PROXY_IDLE_TIMEOUT_MS,
  MAX_TIMER_MS,
  normalizeIdleTimeoutMs,
  resolveCtlProxyIdleTimeout,
  resolveCtlProxyIdleTimeoutMs,
  startDashboardServer,
  type DashboardServerHandle,
} from "./dashboard";

/**
 * Regression lock for #2120. The fix must satisfy BOTH halves at once: a stalled
 * upstream surfaces an error instead of hanging, AND an SSE stream that keeps
 * emitting is not severed at the bound. Pinning only the first would green-light
 * an absolute deadline, which kills `/api/assets/prep/:id/stream`.
 */
describe("proxyToCtl inactivity bound", () => {
  const upstreams: Server[] = [];
  const dashboards: DashboardServerHandle[] = [];

  /** Stand up a stub ctl server on a kernel-assigned loopback port. */
  async function startUpstream(handler: (res: ServerResponse) => void): Promise<{ port: number }> {
    const server = createServer((_req, res) => handler(res));
    upstreams.push(server);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const addr = server.address();
    if (addr === null || typeof addr === "string") throw new Error("no upstream port");
    return { port: addr.port };
  }

  async function startDashboard(
    ctlPort: number,
    ctlProxyIdleTimeoutMs: number,
  ): Promise<DashboardServerHandle> {
    const handle = await startDashboardServer({
      port: 0,
      ctl: { port: ctlPort, token: "test-token" },
      assetsDir: "/nonexistent",
      ctlProxyIdleTimeoutMs,
    });
    dashboards.push(handle);
    return handle;
  }

  afterEach(async () => {
    // Both `closeAllConnections` calls are load-bearing: the stall tests leave a
    // socket open on both hops and `close()` alone waits for them, hanging the
    // hook. Upstream first, so the proxied request settles rather than aborting.
    for (const u of upstreams) {
      u.closeAllConnections();
      await new Promise<void>((r) => u.close(() => r()));
    }
    upstreams.length = 0;
    for (const d of dashboards) {
      d.server.closeAllConnections();
      await d.close();
    }
    dashboards.length = 0;
  });

  it("rejects a stalled upstream instead of hanging", async () => {
    // Accept, then never write — the #2120 condition. The connection SUCCEEDS, so
    // the connect-phase bound in `control/client.ts` is irrelevant here.
    const { port } = await startUpstream(() => {
      /* deliberately silent */
    });
    const dash = await startDashboard(port, 200);

    const res = await fetch(`http://127.0.0.1:${dash.port}/api/healthz-not-local`);
    expect(res.status).toBe(502);
    const body = (await res.json()) as { error: string; ctl: { port: number } };
    // "ctl proxy failed" alone would read to an operator as a refused connection.
    expect(body.error).toContain("inactivity timeout");
    expect(body.error).toContain(`127.0.0.1:${port}`);
    expect(body.ctl.port).toBe(port);
  });

  it("does not sever a live SSE stream that keeps emitting", async () => {
    // The shape that separates the two semantics: every inter-chunk gap is shorter
    // than the bound, but total duration is far longer. Inactivity keeps resetting
    // and all six chunks land; an absolute deadline truncates partway.
    const idleMs = 400;
    const gapMs = 100;
    const chunks = 6; // 600ms total — 1.5x the bound.
    const { port } = await startUpstream((res) => {
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        "x-accel-buffering": "no",
      });
      let sent = 0;
      const tick = setInterval(() => {
        sent += 1;
        res.write(`data: progress-${sent}\n\n`);
        if (sent === chunks) {
          clearInterval(tick);
          res.end();
        }
      }, gapMs);
    });
    const dash = await startDashboard(port, idleMs);

    const res = await fetch(`http://127.0.0.1:${dash.port}/api/assets/prep/abc/stream`);
    expect(res.status).toBe(200);
    // SSE bypass headers must survive the proxy or an intermediary buffers the
    // stream and the operator sees nothing until it ends.
    expect(res.headers.get("content-type")).toContain("text/event-stream");
    expect(res.headers.get("x-accel-buffering")).toBe("no");

    const text = await res.text();
    // The LAST chunk is the load-bearing one — an absolute deadline truncates
    // the body before it.
    for (let i = 1; i <= chunks; i += 1) {
      expect(text).toContain(`progress-${i}`);
    }
  });

  it("aborts a mid-stream stall without crashing the process", async () => {
    // Headers are flushed BEFORE the stall, so an unguarded `error` handler calls
    // `sendJson` on a committed response. That throw is uncaught inside a
    // ClientRequest listener — it takes the whole dashboard process down.
    const uncaught: Error[] = [];
    const onUncaught = (err: Error) => uncaught.push(err);
    process.on("uncaughtException", onUncaught);
    try {
      const { port } = await startUpstream((res) => {
        res.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
        });
        res.write("data: progress-1\n\n");
        // …then silence. The bound fires with headers already sent.
      });
      const dash = await startDashboard(port, 200);

      const res = await fetch(`http://127.0.0.1:${dash.port}/api/assets/prep/abc/stream`);
      // Status was already sent, so the honest outcome is a 200 with a truncated
      // body, not a 502.
      expect(res.status).toBe(200);
      await expect(res.text()).rejects.toThrow();

      // Let the error handler run before asserting.
      await new Promise<void>((r) => setTimeout(r, 100));
      expect(uncaught.map((e) => e.message)).toEqual([]);
    } finally {
      process.off("uncaughtException", onUncaught);
    }
  });

  it("releases the upstream when the client disconnects mid-stream", async () => {
    // The half the inactivity timer cannot reclaim: a live upstream keeps resetting
    // it. On the real prep-stream route, ctl's own `req.on("close")` unsubscribe is
    // waiting on THIS request, so the leak is a subscriber plus two sockets.
    let markClosed: () => void = () => undefined;
    const upstreamClosed = new Promise<void>((r) => {
      markClosed = r;
    });
    const { port } = await startUpstream((res) => {
      const tick = setInterval(() => {
        if (!res.writableEnded) res.write("data: tick\n\n");
      }, 25);
      res.on("close", () => {
        clearInterval(tick);
        markClosed();
      });
    });
    const dash = await startDashboard(port, 5_000);

    const ac = new AbortController();
    // `fetch` resolves on response HEADERS while the body still streams, so the
    // abort must come after this await and it is the body read that rejects.
    const res = await fetch(`http://127.0.0.1:${dash.port}/api/assets/prep/abc/stream`, {
      signal: ac.signal,
    });
    expect(res.status).toBe(200);
    const bodyRead = res.text();
    // Let the stream deliver at least one chunk, then walk away.
    await new Promise<void>((r) => setTimeout(r, 100));
    ac.abort();
    await expect(bodyRead).rejects.toThrow();

    // Observe the release directly on the upstream rather than inferring it from a
    // write counter that stopped: this settles in single-digit ms when the handler
    // works, and only pays the timeout when it does not.
    const outcome = await Promise.race([
      upstreamClosed.then(() => "released" as const),
      new Promise<"still-streaming">((r) => setTimeout(() => r("still-streaming"), 2_000)),
    ]);
    expect(outcome).toBe("released");
  });

  it("clamps an operator override to the largest delay Node can hold", () => {
    // Node clamps a larger socket-timeout delay to this and warns; clamping here
    // keeps the configured value equal to the one that actually runs.
    expect(resolveCtlProxyIdleTimeoutMs(String(MAX_TIMER_MS + 1))).toBe(MAX_TIMER_MS);
    expect(resolveCtlProxyIdleTimeoutMs("999999999999")).toBe(MAX_TIMER_MS);
    // A value inside the range is untouched.
    expect(resolveCtlProxyIdleTimeoutMs(String(MAX_TIMER_MS))).toBe(MAX_TIMER_MS);
  });

  it("defaults to a bound that exceeds any legitimate inter-chunk gap", () => {
    // The default measures the gap BETWEEN chunks, not a request duration —
    // "unifying" it with `ctlRequest`'s 10s breaks that.
    expect(CTL_PROXY_IDLE_TIMEOUT_MS).toBeGreaterThan(60_000);
  });

  it("honors a zero override by disabling the bound entirely", async () => {
    // A stalled upstream with the bound off must NOT settle; a 502 here would mean
    // the override is a no-op.
    const { port } = await startUpstream(() => {
      /* silent */
    });
    const dash = await startDashboard(port, 0);

    const settled = await Promise.race([
      fetch(`http://127.0.0.1:${dash.port}/api/anything`).then(() => "settled" as const),
      new Promise<"still-open">((r) => setTimeout(() => r("still-open"), 400)),
    ]);
    expect(settled).toBe("still-open");
  });

  describe("normalizeIdleTimeoutMs", () => {
    it("keeps the bound armed for a negative passed directly to the option", () => {
      // The resolver guards the env path, but `ctlProxyIdleTimeoutMs` is public API:
      // a negative here would fail `> 0` at the arming site and disable the bound.
      // Clamping to 0 would be the same defect, so these must land on the DEFAULT.
      for (const bad of [-5, -1, -0, Number.NaN, Number.NEGATIVE_INFINITY]) {
        expect(normalizeIdleTimeoutMs(bad)).toBe(CTL_PROXY_IDLE_TIMEOUT_MS);
      }
    });

    it("clamps an over-large option to what Node can actually hold", () => {
      expect(normalizeIdleTimeoutMs(MAX_TIMER_MS + 1)).toBe(MAX_TIMER_MS);
    });

    it("honors an explicit 0 as a deliberate disable", () => {
      expect(normalizeIdleTimeoutMs(0)).toBe(0);
    });
  });

  describe("resolveCtlProxyIdleTimeout ignored flag", () => {
    // The CLI warns off this flag. It must mirror the reject set exactly: a
    // spurious warning on an honored value is as wrong as silence on a dropped one.
    it.each(["abc", "-1", "-0", "1.5", "600000ms", "0x10", "1e3", "+5", "NaN"])(
      "reports %s as ignored",
      (raw) => {
        const r = resolveCtlProxyIdleTimeout(raw);
        expect(r.ignored).toBe(true);
        expect(r.value).toBe(CTL_PROXY_IDLE_TIMEOUT_MS);
      },
    );

    it.each(["1200", "0", " 12 ", undefined, "", "   "])("reports %s as honored", (raw) => {
      expect(resolveCtlProxyIdleTimeout(raw).ignored).toBe(false);
    });

    it("agrees with the value-only form on every input", () => {
      for (const raw of ["abc", "-0", "0", "1200", " 12 ", undefined, "", "1e3"]) {
        expect(resolveCtlProxyIdleTimeout(raw).value).toBe(resolveCtlProxyIdleTimeoutMs(raw));
      }
    });
  });

  describe("resolveCtlProxyIdleTimeoutMs", () => {
    it("passes an explicit override through", () => {
      expect(resolveCtlProxyIdleTimeoutMs("1200")).toBe(1200);
    });

    it("treats an explicit 0 as a deliberate disable", () => {
      expect(resolveCtlProxyIdleTimeoutMs("0")).toBe(0);
    });

    it.each(["", "   ", undefined, "abc", "-1", "1.5", "NaN", "0x10", "1e3", "+5", "600000ms"])(
      "falls back to the armed default for %s",
      (raw) => {
        expect(resolveCtlProxyIdleTimeoutMs(raw)).toBe(CTL_PROXY_IDLE_TIMEOUT_MS);
      },
    );

    // `-0` is the one negative that a numeric guard lets through: `Number("-0")`
    // is `-0`, so `n < 0` is false and `isInteger` is true, but `-0 > 0` is false
    // at the arming site — the bound never arms and the #2120 hang returns.
    it.each(["-0", " -0 ", "-0.0"])("keeps the bound armed for %s", (raw) => {
      const resolved = resolveCtlProxyIdleTimeoutMs(raw);
      expect(resolved).toBe(CTL_PROXY_IDLE_TIMEOUT_MS);
      expect(resolved > 0).toBe(true);
    });
  });
});
