import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { parseDiagPacketsLine } from "./bot";
import {
  buildReceiverConfigOverrides,
  DIAG_PACKETS_CONFIG_KEY,
  DIAG_PACKETS_ENV,
} from "./receiver-caps";

// Drift locks for --diag-packets (#2970): each reads the real file.
const BOTS_APP = resolve(import.meta.dirname, "..");
const REPO_ROOT = resolve(BOTS_APP, "..", "..");
const ATTENDANTS_REL = "dioxus-ui/src/components/attendants.rs";
const CONSTANTS_REL = "dioxus-ui/src/constants.rs";
const readText = (...parts: string[]): string => readFileSync(resolve(...parts), "utf8");

function match(re: RegExp, text: string, what: string): RegExpExecArray {
  const m = re.exec(text);
  expect(m, what).not.toBeNull();
  return m!;
}

const sourceDisplayArms = (): string[] => {
  const body = match(
    /impl std::fmt::Display for DiagnosticsPacketsSource \{([\s\S]*?)\n\}/,
    readText(REPO_ROOT, CONSTANTS_REL),
    "constants.rs no longer implements Display for DiagnosticsPacketsSource",
  )[1];
  return [...body.matchAll(/Self::\w+ => "([^"]+)"/g)].map((m) => m[1]);
};

describe("diag-packets drift locks", () => {
  it("parses every line attendants.rs can print, with the sources constants.rs renders", () => {
    const [, fmt, args] = match(
      /log::info!\(\s*"(diagnostics packets: [^"]*)",([\s\S]*?)\);/,
      readText(REPO_ROOT, ATTENDANTS_REL),
      "attendants.rs no longer logs the `diagnostics packets:` line at info",
    );
    const states = [...args.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
    expect(states).toEqual(["ENABLED", "DISABLED"]);
    const sources = sourceDisplayArms();
    expect(sources.length).toBeGreaterThan(0);
    for (const state of states) {
      for (const source of sources) {
        const line = fmt.replace("{}", state).replace("{}", source);
        expect(parseDiagPacketsLine(line), line).toEqual({ state, source });
      }
    }
  });

  it("injects the serde key of an Option<String> field with a value the client reads as off", () => {
    const rs = readText(REPO_ROOT, CONSTANTS_REL);
    const key = match(
      /#\[serde\(rename = "(\w+)"\)\]\s*(?:#\[serde\(default\)\]\s*)?pub diagnostics_packets_enabled: Option<String>,/,
      rs,
      "RuntimeConfig.diagnostics_packets_enabled is gone or no longer an Option<String>",
    )[1];
    expect(DIAG_PACKETS_CONFIG_KEY).toBe(key);
    const injected = buildReceiverConfigOverrides({ diagPackets: "off" })?.[key];
    expect(typeof injected).toBe("string");
    const parser = match(
      /fn parse_diagnostics_packets_switch\([\s\S]*?\n\}/,
      rs,
      "constants.rs no longer defines parse_diagnostics_packets_switch",
    )[0];
    const offArm = match(
      /((?:"[^"]+"\s*\|\s*)*"[^"]+")\s*=>\s*Some\(false\)/,
      parser,
      "no off arm",
    )[1];
    expect(offArm.split("|").map((t) => t.trim().replace(/"/g, ""))).toContain(injected);
  });

  it("a falsy config value is checked before the URL param (the injection is a ceiling)", () => {
    const body = match(
      /pub fn resolve_diagnostics_packets\([\s\S]*?\n\}/,
      readText(REPO_ROOT, CONSTANTS_REL),
      "constants.rs no longer defines resolve_diagnostics_packets",
    )[0];
    const ceiling = body.indexOf("if config == Some(false)");
    expect(ceiling).toBeGreaterThan(-1);
    expect(ceiling).toBeLessThan(body.indexOf("url_param.and_then"));
  });

  it("the shipped StatefulSet leaves it unset and its opt-in hint names the env the CLI reads", () => {
    const text = readText(BOTS_APP, "k8s", "statefulset.yaml");
    const doc = parseYaml(text) as {
      spec: { template: { spec: { containers: { env?: { name: string }[] }[] } } };
    };
    const live = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []).map((e) => e.name);
    expect(live).not.toContain(DIAG_PACKETS_ENV);
    expect(text).toMatch(new RegExp(`^\\s*#\\s*- name: ${DIAG_PACKETS_ENV}$`, "m"));
  });

  it("re-runs on a change to either Rust file, or a drift lands with this lock unread", () => {
    const wf = parseYaml(
      readText(REPO_ROOT, ".github/workflows/pr-check-e2e-lint-hcl.yaml"),
    ) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toEqual(
      expect.arrayContaining([ATTENDANTS_REL, CONSTANTS_REL]),
    );
  });
});
