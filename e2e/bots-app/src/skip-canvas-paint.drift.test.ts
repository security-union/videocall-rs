import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { resolveSkipCanvasPaint, SKIP_CANVAS_PAINT_ENV } from "./receiver-caps";

describe("k8s/statefulset.yaml skips canvas paint (issue 3086)", () => {
  it("declares the env the CLI reads with a value it resolves to true", () => {
    const path = resolve(import.meta.dirname, "..", "k8s", "statefulset.yaml");
    const doc = parseYaml(readFileSync(path, "utf8")) as {
      spec: { template: { spec: { containers: { env?: { name: string; value?: string }[] }[] } } };
    };
    const env = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []);
    const declared = env.find((e) => e.name === SKIP_CANVAS_PAINT_ENV)?.value;
    expect(declared, `statefulset.yaml must declare ${SKIP_CANVAS_PAINT_ENV}`).toBeDefined();
    expect(resolveSkipCanvasPaint(declared)).toEqual({ kind: "ok", value: true });
  });
});
