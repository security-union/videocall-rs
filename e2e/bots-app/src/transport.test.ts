import vm from "node:vm";

import { describe, expect, it } from "vitest";

import { buildReceiverConfigOverrides } from "./receiver-caps";
import {
  buildTransportInitScript,
  isFallback,
  parseTransportLine,
  resolveTransport,
  type TransportApplied,
  transportMismatch,
} from "./transport";

const ORIGIN = "https://app.example.test";

describe("resolveTransport (issue 3087)", () => {
  it("maps the CLI tokens to the manifest values", () => {
    expect(resolveTransport(undefined)).toEqual({ kind: "ok", value: "auto" });
    expect(resolveTransport(" ")).toEqual({ kind: "ok", value: "auto" });
    expect(resolveTransport("AUTO")).toEqual({ kind: "ok", value: "auto" });
    expect(resolveTransport(" ws ")).toEqual({ kind: "ok", value: "websocket" });
    expect(resolveTransport("WT")).toEqual({ kind: "ok", value: "webtransport" });
    expect(resolveTransport("websocket")).toEqual({ kind: "ok", value: "websocket" });
    expect(resolveTransport("quic").kind).toBe("invalid");
  });

  it("injects the pinned value as defaultTransport alongside the other overrides", () => {
    expect(buildReceiverConfigOverrides({ defaultTransport: "websocket" })).toEqual({
      defaultTransport: "websocket",
    });
    expect(buildReceiverConfigOverrides({ defaultTransport: undefined })).toBeNull();
  });
});

describe("buildTransportInitScript (issue 3087)", () => {
  function run(origin: string): string[] {
    const calls: string[] = [];
    const storage = (area: string) => ({
      removeItem: (k: string) => calls.push(`${area}:${k}`),
    });
    vm.runInNewContext(buildTransportInitScript(ORIGIN), {
      location: { origin },
      localStorage: storage("local"),
      sessionStorage: storage("session"),
      console,
    });
    return calls;
  }

  it("removes all three remembered-choice keys on the app origin", () => {
    expect(run(ORIGIN)).toEqual([
      "local:vc_transport_sticky",
      "local:vc_transport_preference",
      "session:vc_transport_session",
    ]);
  });

  it("touches nothing on another origin", () => {
    expect(run("https://id.example.test")).toEqual([]);
  });
});

describe("parseTransportLine (issue 3087)", () => {
  it("reads the applied preference with its source and URL counts", () => {
    expect(
      parseTransportLine(
        "INFO src/components/attendants.rs:1335 Transport preference applied: pref=websocket source=default wt_urls=0 ws_urls=2",
      ),
    ).toEqual({
      kind: "applied",
      applied: { preference: "websocket", source: "default", wt_urls: 0, ws_urls: 2 },
    });
  });

  it("reads the active transport of an election, including a re-election generation", () => {
    const line = (active: string): string =>
      `Election decision: reason=x rtt_lane=reliable outcome=elected elected=wt_0 active=${active} wt_samples=3`;
    expect(parseTransportLine(line("ws_0"))).toEqual({ kind: "elected", active: "websocket" });
    expect(parseTransportLine(line("wt_1_g2"))).toEqual({
      kind: "elected",
      active: "webtransport",
    });
    expect(parseTransportLine(line("none"))).toEqual({ kind: "elected", active: "none" });
    expect(parseTransportLine(line("xx_0"))).toBeNull();
    expect(parseTransportLine("diagnostics packets: ENABLED (source=default)")).toBeNull();
  });
});

const applied = (over: Partial<TransportApplied> = {}): TransportApplied => ({
  preference: "webtransport",
  source: "default",
  wt_urls: 1,
  ws_urls: 1,
  ...over,
});

describe("transportMismatch (issue 3087)", () => {
  it("accepts the pinned value applied from the default", () => {
    expect(transportMismatch("webtransport", applied())).toBeNull();
    expect(transportMismatch("websocket", applied({ preference: "websocket" }))).toBeNull();
  });

  it("refuses a remembered choice even when it names the pinned value", () => {
    for (const source of ["sticky", "session"]) {
      expect(transportMismatch("webtransport", applied({ source }))).toContain(`source=${source}`);
    }
  });

  it("refuses a default that is not the pinned value", () => {
    expect(transportMismatch("websocket", applied())).toContain("pref=webtransport");
  });
});

describe("isFallback (issue 3087)", () => {
  it("counts only a webtransport-to-websocket move while webtransport was preferred and offered", () => {
    expect(isFallback("webtransport", "websocket", applied())).toBe(true);
    expect(isFallback(null, "websocket", applied())).toBe(false);
    expect(isFallback("websocket", "websocket", applied())).toBe(false);
    expect(isFallback("webtransport", "webtransport", applied())).toBe(false);
    expect(isFallback("webtransport", "none", applied())).toBe(false);
    expect(isFallback("webtransport", "websocket", applied({ wt_urls: 0 }))).toBe(false);
    expect(isFallback("webtransport", "websocket", applied({ preference: "websocket" }))).toBe(
      false,
    );
    expect(isFallback("webtransport", "websocket", null)).toBe(false);
  });

  it("counts a websocket-to-webtransport move while websocket was preferred and both were offered", () => {
    const ws = (over: Partial<TransportApplied> = {}) =>
      applied({ preference: "websocket", ...over });
    expect(isFallback("websocket", "webtransport", ws())).toBe(true);
    expect(isFallback(null, "webtransport", ws())).toBe(false);
    expect(isFallback("websocket", "webtransport", applied())).toBe(false);
    expect(isFallback("websocket", "webtransport", ws({ wt_urls: 0 }))).toBe(false);
    expect(isFallback("websocket", "none", ws())).toBe(false);
  });
});
