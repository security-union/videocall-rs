import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { resolveTransportWsEvery, websocketIndices } from "./posture";
import { buildReceiverConfigOverrides } from "./receiver-caps";
import {
  DEFAULT_TRANSPORT_CONFIG_KEY,
  parseTransportLine,
  resolveTransport,
  TRANSPORT_ENV,
  TRANSPORT_PREF_KEY,
  TRANSPORT_SESSION_KEY,
  TRANSPORT_STICKY_KEY,
  TRANSPORT_WS_EVERY_ENV,
  transportMismatch,
} from "./transport";

// Drift locks for --transport (issue 3087): each reads the real file.
const BOTS_APP = resolve(import.meta.dirname, "..");
const REPO_ROOT = resolve(BOTS_APP, "..", "..");
const CONTEXT_REL = "dioxus-ui/src/context.rs";
const CONSTANTS_REL = "dioxus-ui/src/constants.rs";
const ATTENDANTS_REL = "dioxus-ui/src/components/attendants.rs";
const APPLIED_SITES = [
  ATTENDANTS_REL,
  "dioxus-ui/src/components/waiting_room.rs",
  "dioxus-ui/src/pages/meeting.rs",
  "dioxus-ui/src/pages/guest_join.rs",
];
const CONN_MGR_REL = "videocall-client/src/connection/connection_manager.rs";
const readText = (...parts: string[]): string => readFileSync(resolve(...parts), "utf8");

function match(re: RegExp, text: string, what: string): RegExpExecArray {
  const m = re.exec(text);
  expect(m, what).not.toBeNull();
  return m!;
}

describe("transport drift locks", () => {
  const context = readText(REPO_ROOT, CONTEXT_REL);

  it("clears the storage keys context.rs reads", () => {
    const key = (name: string): string =>
      match(new RegExp(`const ${name}: &str = "([^"]+)";`), context, `${name} is gone`)[1];
    expect(TRANSPORT_PREF_KEY).toBe(key("TRANSPORT_PREF_KEY"));
    expect(TRANSPORT_STICKY_KEY).toBe(key("TRANSPORT_STICKY_KEY"));
    expect(TRANSPORT_SESSION_KEY).toBe(key("TRANSPORT_SESSION_KEY"));
  });

  it("fails a pinned bot on every source except the config default", () => {
    const body = match(
      /pub fn load_transport_preference_with_source\(\)[\s\S]*?\n\}/,
      context,
      "context.rs no longer defines load_transport_preference_with_source",
    )[0];
    const sources = [...new Set([...body.matchAll(/, "(\w+)"\)/g)].map((m) => m[1]))];
    expect(sources).toEqual(expect.arrayContaining(["sticky", "session", "default"]));
    for (const source of sources) {
      const verdict = transportMismatch("websocket", {
        preference: "websocket",
        source,
        wt_urls: 0,
        ws_urls: 1,
      });
      expect(verdict === null, source).toBe(source === "default");
    }
  });

  it("injects the serde key with a value the client's TransportPreference parses", () => {
    const key = match(
      /#\[serde\(rename = "(\w+)"\)\]\s*(?:#\[serde\(default\)\]\s*)?pub default_transport: Option<String>,/,
      readText(REPO_ROOT, CONSTANTS_REL),
      "RuntimeConfig.default_transport is gone or no longer an Option<String>",
    )[1];
    expect(DEFAULT_TRANSPORT_CONFIG_KEY).toBe(key);
    const display = match(
      /impl std::fmt::Display for TransportPreference \{([\s\S]*?)\n\}/,
      context,
      "context.rs no longer implements Display for TransportPreference",
    )[1];
    const arms = [...display.matchAll(/=> "([^"]+)"/g)].map((m) => m[1]).sort();
    const injected = ["ws", "wt"].map((t) => {
      const r = resolveTransport(t);
      if (r.kind !== "ok" || r.value === "auto") throw new Error(t);
      return buildReceiverConfigOverrides({ defaultTransport: r.value })?.[key];
    });
    expect(injected.sort()).toEqual(arms);
  });

  it.each(APPLIED_SITES)("parses the applied line at every %s site", (rel) => {
    const fmts = [
      ...readText(REPO_ROOT, rel).matchAll(/"(Transport preference applied: [^"]*)"/g),
    ].map((m) => m[1]);
    expect(fmts.length).toBeGreaterThan(0);
    for (const fmt of fmts) {
      const line = ["websocket", "default", "0", "2"].reduce((s, v) => s.replace("{}", v), fmt);
      expect(parseTransportLine(line), fmt).toEqual({
        kind: "applied",
        applied: { preference: "websocket", source: "default", wt_urls: 0, ws_urls: 2 },
      });
    }
  });

  it("reads active= from the election line with the connection-id prefixes the manager mints", () => {
    const rs = readText(REPO_ROOT, CONN_MGR_REL);
    const fmt = match(
      /fn format_election_decision\([\s\S]*?"(Election decision: [\s\S]*?)",/,
      rs,
      "connection_manager.rs no longer formats `Election decision:`",
    )[1].replace(/\\\n\s*/g, "");
    const [, wt, ws] = match(
      /if is_wt \{\s*\("(\w+)", "WebTransport"\)\s*\} else \{\s*\("(\w+)", "WebSocket"\)/,
      rs,
      "connection_manager.rs no longer picks the wt/ws connection-id prefix",
    );
    const line = (active: string): string =>
      fmt.replace("active={}", `active=${active}`).replace(/\{[^}]*\}/g, "1");
    expect(parseTransportLine(line(`${ws}_0`))).toEqual({ kind: "elected", active: "websocket" });
    expect(parseTransportLine(line(`${wt}_1_g2`))).toEqual({
      kind: "elected",
      active: "webtransport",
    });
  });

  it("re-runs on a change to any parsed Rust file", () => {
    const wf = parseYaml(
      readText(REPO_ROOT, ".github/workflows/pr-check-e2e-lint-hcl.yaml"),
    ) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toEqual(
      expect.arrayContaining([CONTEXT_REL, CONSTANTS_REL, CONN_MGR_REL, ...APPLIED_SITES]),
    );
  });

  it("the shipped StatefulSet sets BOT_TRANSPORT_WS_EVERY=5, which the resolver accepts and which puts a bot on WebSocket at its BOT_FLEET_SIZE, and no BOT_TRANSPORT", () => {
    const doc = parseYaml(readText(BOTS_APP, "k8s", "statefulset.yaml")) as {
      spec: {
        template: { spec: { containers: { env?: { name: string; value?: string }[] }[] } };
      };
    };
    const env = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []);
    expect(env.map((e) => e.name)).not.toContain(TRANSPORT_ENV);
    const stride = env.filter((e) => e.name === TRANSPORT_WS_EVERY_ENV).map((e) => e.value);
    expect(stride).toEqual(["5"]);
    const resolved = resolveTransportWsEvery(stride[0]);
    if (resolved.kind !== "ok" || resolved.value === null)
      throw new Error(JSON.stringify(resolved));
    expect(websocketIndices(resolved.value, 8)).toEqual([2, 7]);
    const fleetSize = env.filter((e) => e.name === "BOT_FLEET_SIZE").map((e) => e.value);
    expect(fleetSize, "statefulset.yaml must declare BOT_FLEET_SIZE once").toHaveLength(1);
    expect(
      websocketIndices(resolved.value, Number(fleetSize[0])),
      `no WebSocket bot at the shipped BOT_FLEET_SIZE=${fleetSize[0]}`,
    ).not.toEqual([]);
  });
});
