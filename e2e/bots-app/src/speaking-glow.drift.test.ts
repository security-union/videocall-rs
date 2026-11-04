import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { SPEAKING_GLOW_OFF_VALUE, SPEAKING_GLOW_STORAGE_KEY } from "./speaking-glow";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");
const CONTEXT_REL = "dioxus-ui/src/context.rs";
const readText = (rel: string): string => readFileSync(resolve(REPO_ROOT, rel), "utf8");

describe("speaking-glow drift locks", () => {
  it("seeds the key the client loads, with the one value it reads as glow off", () => {
    const rs = readText(CONTEXT_REL);
    const key = /const APPEARANCE_GLOW_ENABLED_STORAGE_KEY: &str = "([^"]+)";/.exec(rs);
    expect(key, "context.rs no longer defines APPEARANCE_GLOW_ENABLED_STORAGE_KEY").not.toBeNull();
    expect(SPEAKING_GLOW_STORAGE_KEY).toBe(key![1]);
    const loader = /pub fn load_appearance_settings_from_storage\(\)[\s\S]*?\n\}/.exec(rs);
    expect(
      loader,
      "context.rs no longer defines load_appearance_settings_from_storage",
    ).not.toBeNull();
    const parse =
      /read_local_storage\(APPEARANCE_GLOW_ENABLED_STORAGE_KEY\)\s*\{\s*settings\.glow_enabled = value != "([^"]+)";/.exec(
        loader![0],
      );
    expect(parse, 'the loader no longer parses the glow key as `value != "..."`').not.toBeNull();
    expect(SPEAKING_GLOW_OFF_VALUE).toBe(parse![1]);
  });

  it("re-runs on a change to context.rs, or a drift lands with this lock unread", () => {
    const wf = parseYaml(readText(".github/workflows/pr-check-e2e-lint-hcl.yaml")) as Record<
      string,
      unknown
    >;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toEqual(expect.arrayContaining([CONTEXT_REL]));
  });
});
