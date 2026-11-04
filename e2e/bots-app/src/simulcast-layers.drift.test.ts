import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import {
  SIMULCAST_LADDER_DEPTH,
  SIMULCAST_MAX_LAYERS_CONFIG_KEY,
  SIMULCAST_MAX_LAYERS_ENV,
} from "./simulcast-layers";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..", "..");
const readText = (rel: string): string => readFileSync(resolve(REPO_ROOT, rel), "utf8");

describe("simulcast max layers drift locks (#3246)", () => {
  it("injects the key the client deserializes, as the number type it declares", () => {
    const m =
      /#\[serde\(rename = "([^"]+)"\)\]\s*#\[serde\(default = "default_experimental_simulcast_max_layers"\)\]\s*pub experimental_simulcast_max_layers: (\w+),/.exec(
        readText("dioxus-ui/src/constants.rs"),
      );
    expect(m, "constants.rs no longer declares experimental_simulcast_max_layers").not.toBeNull();
    expect(m![1]).toBe(SIMULCAST_MAX_LAYERS_CONFIG_KEY);
    expect(m![2]).toBe("u32");
  });

  it("caps N at the video ladder depth, which is also the client default", () => {
    const depth = /pub const SIMULCAST_MAX_LAYERS: usize = (\d+);/.exec(
      readText("videocall-aq/src/constants.rs"),
    );
    expect(depth, "videocall-aq no longer defines SIMULCAST_MAX_LAYERS").not.toBeNull();
    expect(SIMULCAST_LADDER_DEPTH).toBe(Number(depth![1]));
    const dflt = /fn default_experimental_simulcast_max_layers\(\) -> u32 \{\s*(\d+)\s*\}/.exec(
      readText("dioxus-ui/src/constants.rs"),
    );
    expect(dflt, "constants.rs default_experimental_simulcast_max_layers moved").not.toBeNull();
    expect(Number(dflt![1])).toBe(SIMULCAST_LADDER_DEPTH);
  });

  it("k8s/statefulset.yaml leaves the env unset, so the fleet inherits the target's ceiling", () => {
    const doc = parseYaml(readText("e2e/bots-app/k8s/statefulset.yaml")) as {
      spec: { template: { spec: { containers: { env?: { name: string }[] }[] } } };
    };
    const env = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []);
    expect(env.length).toBeGreaterThan(0);
    expect(env.map((e) => e.name)).not.toContain(SIMULCAST_MAX_LAYERS_ENV);
  });
});
