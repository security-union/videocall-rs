import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { resolveFleetSize } from "./posture";

interface StatefulSet {
  spec: {
    replicas: number;
    template: { spec: { containers: { env: { name: string; value?: string }[] }[] } };
  };
}

describe("k8s/statefulset.yaml talking-clock rotation (issue 3072)", () => {
  it("sets BOT_FLEET_SIZE to its own replica count", () => {
    const path = resolve(import.meta.dirname, "..", "k8s", "statefulset.yaml");
    const doc = parseYaml(readFileSync(path, "utf8")) as StatefulSet;
    const env = doc.spec.template.spec.containers[0].env;
    const declared = env.find((e) => e.name === "BOT_FLEET_SIZE")?.value;
    expect(declared, "statefulset.yaml must declare BOT_FLEET_SIZE").toBeDefined();
    expect(resolveFleetSize(undefined, declared, 0)).toEqual({
      kind: "ok",
      value: doc.spec.replicas,
    });
  });
});
