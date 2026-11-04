import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { PERSONA_ENV } from "./persona";
import { parseHostRender } from "./persona-driver";

// Drift locks for personas (issue 3091): each reads the real file.
const BOTS_APP = resolve(import.meta.dirname, "..");
const REPO_ROOT = resolve(BOTS_APP, "..", "..");
const HOST_REL = "dioxus-ui/src/components/host.rs";
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const readText = (...parts: string[]): string => readFileSync(resolve(...parts), "utf8");
const ENV_NAMES = Object.values(PERSONA_ENV);
const named = (text: string): string[] =>
  [
    ...new Set([...text.matchAll(/\b(?:BOT_PERSONAS?(?:_\w+)?|BOT_CAMERA)\b/g)].map((m) => m[0])),
  ].sort();

describe("persona drift locks", () => {
  it("parses the Host render line host.rs logs", () => {
    const fmt =
      /"(Host render: video=\{video_enabled\} prev=\{\} mic=\{mic_enabled\} prev=\{\} screen=\{share_screen\} prev=\{\})"/.exec(
        readText(REPO_ROOT, HOST_REL),
      );
    expect(
      fmt,
      "host.rs no longer logs the Host render state line this parser reads",
    ).not.toBeNull();
    const line = (video: boolean, pv: boolean, mic: boolean, pm: boolean): string => {
      const prev = [pv, pm, false];
      return fmt![1]
        .replace("{video_enabled}", String(video))
        .replace("{mic_enabled}", String(mic))
        .replace("{share_screen}", "false")
        .replace(/\{\}/g, () => String(prev.shift()));
    };
    expect(parseHostRender(line(true, false, false, false))).toEqual([
      { kind: "camera", on: true },
    ]);
    expect(parseHostRender(line(true, true, false, true))).toEqual([{ kind: "mic", on: false }]);
  });

  it("the lint workflow re-runs these tests when host.rs changes", () => {
    const wf = parseYaml(readText(REPO_ROOT, PR_CHECK_REL)) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    expect(on?.pull_request?.paths ?? []).toContain(HOST_REL);
  });

  it("the entrypoint and the StatefulSet name exactly the env vars persona.ts reads", () => {
    expect(named(readText(BOTS_APP, "docker-entrypoint.sh"))).toEqual([...ENV_NAMES].sort());
    expect(named(readText(BOTS_APP, "k8s", "statefulset.yaml"))).toEqual([...ENV_NAMES].sort());
  });

  // Personas stay opt-in until a pod acceptance run (issue 3091).
  it("the shipped StatefulSet sets none of them", () => {
    const doc = parseYaml(readText(BOTS_APP, "k8s", "statefulset.yaml")) as {
      spec: { template: { spec: { containers: { env?: { name: string }[] }[] } } };
    };
    const live = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []).map((e) => e.name);
    expect(live.filter((n) => (ENV_NAMES as string[]).includes(n))).toEqual([]);
  });
});
