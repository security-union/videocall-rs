import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { HD_SOURCE, SD_SOURCE, sourceGeometryForIndex } from "./posture";
import {
  effectiveVideoNoise,
  resolveVideoNoise,
  VIDEO_NOISE_ENV,
  videoNoiseFor,
  videoNoiseLine,
} from "./video-noise";

describe("video noise alpha (issue 3089)", () => {
  it("defaults to 0.1 on a 640x480 source and 0.15 on 1280x720, by fleet index", () => {
    expect(videoNoiseFor(sourceGeometryForIndex(0))).toBe(0.1);
    expect(videoNoiseFor(sourceGeometryForIndex(1))).toBe(0.15);
    expect(videoNoiseFor(sourceGeometryForIndex(2))).toBe(0.1);
    expect(videoNoiseFor(sourceGeometryForIndex(7))).toBe(0.15);
  });

  it("lets an override win, including 0, and falls back to the geometry when unset", () => {
    expect(effectiveVideoNoise({ sourceGeometry: HD_SOURCE })).toBe(0.15);
    expect(effectiveVideoNoise({ videoNoise: null, sourceGeometry: SD_SOURCE })).toBe(0.1);
    expect(effectiveVideoNoise({ videoNoise: 0, sourceGeometry: HD_SOURCE })).toBe(0);
    expect(effectiveVideoNoise({ videoNoise: 0.3, sourceGeometry: SD_SOURCE })).toBe(0.3);
  });

  it.each<[string | undefined, number | undefined]>([
    [undefined, undefined],
    ["", undefined],
    ["  ", undefined],
    ["0", 0],
    ["0.1", 0.1],
    [" .15 ", 0.15],
    ["1", 1],
    ["1.0", 1],
  ])("accepts %j as %s", (raw, value) => {
    expect(resolveVideoNoise(raw)).toEqual({ kind: "ok", value });
  });

  it.each(["1.01", "2", "-0.1", "0.1junk", "1e-1", "0x1", "NaN", "Infinity", "."])(
    "rejects %j, naming the env",
    (raw) => {
      const r = resolveVideoNoise(raw);
      expect(r.kind).toBe("invalid");
      expect(r.kind === "invalid" && r.message).toContain(
        `--video-noise (or ${VIDEO_NOISE_ENV}) must be a number from 0 to 1, got "${raw}"`,
      );
    },
  );

  it("names the alpha and whether it is the geometry default in the per-bot line", () => {
    expect(videoNoiseLine({ sourceGeometry: HD_SOURCE })).toBe(
      "video noise: alpha 0.15 (default for 1280x720)",
    );
    expect(videoNoiseLine({ videoNoise: 0, sourceGeometry: SD_SOURCE })).toBe(
      "video noise: alpha 0 (override; default for 640x480 is 0.1)",
    );
  });

  it("the shipped StatefulSet leaves it unset and its hint names the env the CLI reads", () => {
    const text = readFileSync(
      resolve(import.meta.dirname, "..", "k8s", "statefulset.yaml"),
      "utf8",
    );
    const doc = parseYaml(text) as {
      spec: { template: { spec: { containers: { env?: { name: string }[] }[] } } };
    };
    const live = doc.spec.template.spec.containers.flatMap((c) => c.env ?? []).map((e) => e.name);
    expect(live).not.toContain(VIDEO_NOISE_ENV);
    expect(text).toMatch(new RegExp(`^\\s*#\\s*- name: ${VIDEO_NOISE_ENV}\\b`, "m"));
  });
});
