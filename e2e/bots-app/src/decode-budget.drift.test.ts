import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { DECODE_BUDGET_OFF_VALUE, DECODE_BUDGET_STORAGE_KEY } from "./decode-budget";

// Drift locks for --decode-budget off (#2914): each reads the real client source.
const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");
const CONTEXT_REL = "dioxus-ui/src/context.rs";
const ATTENDANTS_REL = "dioxus-ui/src/components/attendants.rs";
const readText = (rel: string): string => readFileSync(resolve(REPO_ROOT, rel), "utf8");

function match(re: RegExp, text: string, what: string): RegExpExecArray {
  const m = re.exec(text);
  expect(m, what).not.toBeNull();
  return m!;
}

describe("decode-budget drift locks", () => {
  it("seeds the key the client loads, with the value it parses as All", () => {
    const rs = readText(CONTEXT_REL);
    const key = match(
      /const DECODE_BUDGET_OVERRIDE_KEY: &str = "([^"]+)";/,
      rs,
      "context.rs no longer defines DECODE_BUDGET_OVERRIDE_KEY",
    )[1];
    expect(DECODE_BUDGET_STORAGE_KEY).toBe(key);
    const loader = match(
      /pub fn load_decode_budget_override\(\)[\s\S]*?\n\}/,
      rs,
      "context.rs no longer defines load_decode_budget_override",
    )[0];
    expect(loader).toMatch(/local_storage\(\)[\s\S]*get_item\(DECODE_BUDGET_OVERRIDE_KEY\)/);
    const parser = match(
      /fn parse_decode_budget_override\([\s\S]*?\n\}/,
      rs,
      "context.rs no longer defines parse_decode_budget_override",
    )[0];
    const allArm = match(
      /"([^"]+)"\s*=>\s*DecodeBudgetOverride::All,/,
      parser,
      "parse_decode_budget_override has no arm for DecodeBudgetOverride::All",
    )[1];
    expect(DECODE_BUDGET_OFF_VALUE).toBe(allArm);
  });

  it("All skips the adaptive path: no cascade or ProtectiveMode writer runs before the forced-cap continue", () => {
    const rs = readText(ATTENDANTS_REL);
    expect(rs).toMatch(/use_signal\(load_decode_budget_override\)/);
    const forced = match(
      /let forced_cap: Option<usize> = match current_override \{([\s\S]*?)\n\s*\};\s*if let Some\(forced\) = forced_cap \{([\s\S]*?)\n\s*\}\s*\/\/ ---- Auto path ----/,
      rs,
      "attendants.rs no longer bypasses the Auto path on a forced cap",
    );
    expect(forced[1]).toMatch(/DecodeBudgetOverride::All => Some\(/);
    expect(forced[2]).toMatch(/\bcontinue;/);
    const loopStart = rs.indexOf("let mut decode_budget_task");
    const autoPath = rs.indexOf("// ---- Auto path ----");
    expect(loopStart).toBeGreaterThan(-1);
    expect(autoPath).toBeGreaterThan(loopStart);
    const beforeAuto = rs.slice(loopStart, autoPath);
    for (const writer of [
      "tick_protective_mode(",
      "cascade_action(",
      "protective_mode_report.set(",
      "decode_budget_pressured.set(true)",
    ]) {
      expect(beforeAuto, `${writer} runs before the forced-cap continue`).not.toContain(writer);
      expect(rs.indexOf(writer, autoPath), `${writer} is gone from the Auto path`).toBeGreaterThan(
        autoPath,
      );
    }
  });

  it("re-runs on a change to either Rust file, or a drift lands with this lock unread", () => {
    const wf = parseYaml(readText(".github/workflows/pr-check-e2e-lint-hcl.yaml")) as Record<
      string,
      unknown
    >;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toEqual(
      expect.arrayContaining([CONTEXT_REL, ATTENDANTS_REL]),
    );
  });
});
