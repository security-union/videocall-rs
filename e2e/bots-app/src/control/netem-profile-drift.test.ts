import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

import {
  ingressNetemParams,
  NETEM_IFB_DEV,
  NETEM_IFB_TXQUEUELEN,
  NETEM_PROFILE_NAMES,
  NETEM_PROFILES,
  type NetemParams,
} from "./netem";

/** Locks NETEM_PROFILES to `videocall-netsim`'s presets in BOTH directions. */
const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..", "..");
const PROFILES_REL = "videocall-netsim/src/profiles.rs";
const PROFILES = resolve(REPO_ROOT, PROFILES_REL);
const PR_CHECK = resolve(REPO_ROOT, ".github", "workflows", "pr-check-e2e-lint-hcl.yaml");
const README = resolve(import.meta.dirname, "..", "..", "README.md");
const readme = (): string => readFileSync(README, "utf8");

/** The `NetworkProfile { … }` initialiser for one preset name. */
function preset(name: string): Record<string, number> {
  const src = readFileSync(PROFILES, "utf8");
  const block = new RegExp(`"${name}" => Some\\(NetworkProfile \\{([\\s\\S]*?)\\}\\)`).exec(src);
  expect(block, `"${name}" must be findable in ${PROFILES_REL}`).not.toBeNull();
  const out: Record<string, number> = {};
  for (const [, field, value] of block![1].matchAll(
    /(\w+):\s*(?:Some\()?([0-9_]+(?:\.[0-9]+)?)\)?,/g,
  )) {
    out[field] = Number(value.replace(/_/g, ""));
  }
  expect(Object.keys(out).length, `${name} must parse to at least one field`).toBeGreaterThan(0);
  return out;
}

/** netsim field → the NetemParams field that must carry the same number. */
const FIELDS: Array<[string, keyof NetemParams]> = [
  ["latency_ms", "delayMs"],
  ["jitter_ms", "jitterMs"],
  ["loss_pct", "lossPct"],
  ["uplink_kbps", "rateKbit"],
  ["downlink_kbps", "downlinkRateKbit"],
];

const SHAPING = Object.entries(NETEM_PROFILES).filter(
  (e): e is [string, NetemParams] => e[1] !== null,
);

describe("NETEM_PROFILES vs videocall-netsim's presets (#2353)", () => {
  it("re-runs on a profiles.rs change, or a preset edit lands with this lock unread", () => {
    const wf = parseYaml(readFileSync(PR_CHECK, "utf8")) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toContain(PROFILES_REL);
  });

  it.each(SHAPING)("mirrors %s in both directions", (name, params) => {
    const rust = preset(name);
    for (const [rustField, tsField] of FIELDS) {
      expect(rust[rustField], `${name}.${rustField} must be findable`).toBeTypeOf("number");
      expect(params[tsField], `${name}.${tsField} has drifted from ${rustField}`).toBe(
        rust[rustField],
      );
    }
  });

  it("shapes ingress no tighter than the profile's own downlink", () => {
    for (const [name, params] of SHAPING) {
      expect(params.downlinkRateKbit, name).toBeGreaterThanOrEqual(params.rateKbit!);
    }
  });

  it("keeps the README's queue-depth claims true in code", () => {
    expect(readme()).toContain("The ingress queue depth is the SAME `limitPkts` as egress");
    expect(readme()).toContain(
      `\`${NETEM_IFB_DEV}\` device queue is raised from its default 32 to ${NETEM_IFB_TXQUEUELEN}`,
    );
    for (const [name, params] of SHAPING) {
      expect(ingressNetemParams(params).limitPkts, name).toBe(params.limitPkts);
    }
  });

  // A fresh ifb comes up at 32 and that queue sits AHEAD of the netem qdisc, so
  // a depth below any profile's limit makes the device, not the profile, decide.
  it("keeps the ifb device queue above every profile's netem limit", () => {
    for (const [name, params] of SHAPING) {
      expect(NETEM_IFB_TXQUEUELEN, name).toBeGreaterThanOrEqual(params.limitPkts!);
    }
  });

  it("pins the downlink/uplink spread the README and netem.ts both quote", () => {
    const ratios = SHAPING.map(([, p]) => p.downlinkRateKbit! / p.rateKbit!).filter((r) => r > 1);
    expect(ratios.length, "at least one profile must differ between directions").toBeGreaterThan(0);
    const round = (x: number): string => String(Math.round(x * 10) / 10);
    const spread = `${round(Math.min(...ratios))}-${round(Math.max(...ratios))}x`;
    expect(readme(), `the README's stated spread is not ${spread}`).toContain(spread);
    const src = readFileSync(resolve(import.meta.dirname, "netem.ts"), "utf8");
    expect(src, `netem.ts's stated spread is not ${spread}`).toContain(
      spread.replace("-", "\u2013"),
    );
  });

  /**
   * Presets netem cannot express, so `BOT_NETEM_PROFILE=<name>` refuses to
   * start rather than shaping something else. Naming them here is what makes a
   * NEW netsim preset fail this lock instead of landing green.
   */
  const NETSIM_ONLY = ["crushed_downlink"];

  it("covers every netsim preset, or names it as deliberately netsim-only", () => {
    const src = readFileSync(PROFILES, "utf8");
    const block = /pub const PRESET_NAMES: &\[&str\] = &\[([\s\S]*?)\];/.exec(src);
    expect(block, `PRESET_NAMES must be findable in ${PROFILES_REL}`).not.toBeNull();
    const presets = [...block![1].matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1]);
    expect(presets.length, "PRESET_NAMES must parse to at least one name").toBeGreaterThan(0);
    const missing = presets.filter(
      (n) => !NETEM_PROFILE_NAMES.includes(n) && !NETSIM_ONLY.includes(n),
    );
    expect(
      missing,
      "BOT_NETEM_PROFILE would exit 1 on these; add them to NETEM_PROFILES or to NETSIM_ONLY",
    ).toEqual([]);
    // Kept honest in the other direction too: a stale exemption is drift.
    expect(NETSIM_ONLY.filter((n) => !presets.includes(n))).toEqual([]);
    expect(NETSIM_ONLY.filter((n) => NETEM_PROFILE_NAMES.includes(n))).toEqual([]);
  });

  it("keeps `none` an alias for the passthrough preset, not a shaping one", () => {
    expect(NETEM_PROFILES.none).toBeNull();
    expect(readFileSync(PROFILES, "utf8")).toContain(
      '"none" => Some(NetworkProfile::passthrough())',
    );
  });
});
