import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { resolveSpeechPlan, SPEECH_ENV_NAMES, SPEECH_PLAN_DEFAULT } from "./speech-plan";

const BOTS_APP = resolve(import.meta.dirname, "..");

describe("resolveSpeechPlan (issue 3090)", () => {
  it("defaults to the weighted model when nothing is set", () => {
    expect(resolveSpeechPlan({})).toEqual({ kind: "ok", value: SPEECH_PLAN_DEFAULT });
    expect(resolveSpeechPlan({ BOT_SPEECH_MODEL: " " })).toEqual({
      kind: "ok",
      value: SPEECH_PLAN_DEFAULT,
    });
  });

  it("reads every knob, trimmed and case-insensitive on the model", () => {
    expect(
      resolveSpeechPlan({
        BOT_SPEECH_MODEL: " Weighted ",
        BOT_SPEECH_SEED: "4294967295",
        BOT_SPEECH_DOMINANT: "0.5, .25",
        BOT_SPEECH_OBSERVERS: "3",
        BOT_SPEECH_KEEP: "0",
        BOT_SPEECH_OVERLAP: "1",
      }),
    ).toEqual({
      kind: "ok",
      value: {
        model: "weighted",
        seed: 4294967295,
        dominant: [0.5, 0.25],
        observers: 3,
        keep: 0,
        overlap: 1,
      },
    });
    expect(resolveSpeechPlan({ BOT_SPEECH_DOMINANT: "0.4" })).toMatchObject({
      value: { dominant: [0.4] },
    });
  });

  it("selects the strict rotation, and refuses weighted knobs that it would ignore", () => {
    expect(resolveSpeechPlan({ BOT_SPEECH_MODEL: "rotation" })).toEqual({
      kind: "ok",
      value: { model: "rotation" },
    });
    const r = resolveSpeechPlan({ BOT_SPEECH_MODEL: "rotation", BOT_SPEECH_KEEP: "0.6" });
    expect(r).toMatchObject({ kind: "invalid" });
    expect(r.kind === "invalid" && r.message).toContain("BOT_SPEECH_KEEP has no effect");
  });

  it.each([
    ["BOT_SPEECH_MODEL", "round-robin"],
    ["BOT_SPEECH_SEED", "-1"],
    ["BOT_SPEECH_SEED", "4294967296"],
    ["BOT_SPEECH_SEED", "1e3"],
    ["BOT_SPEECH_DOMINANT", "0.3,0.2,0.1"],
    ["BOT_SPEECH_DOMINANT", "0.6,0.4"],
    ["BOT_SPEECH_DOMINANT", "0"],
    ["BOT_SPEECH_DOMINANT", "1"],
    ["BOT_SPEECH_DOMINANT", "0.3,"],
    ["BOT_SPEECH_DOMINANT", "1e-1"],
    ["BOT_SPEECH_OBSERVERS", "2.5"],
    ["BOT_SPEECH_KEEP", "1"],
    ["BOT_SPEECH_KEEP", "0.12345"],
    ["BOT_SPEECH_KEEP", "NaN"],
    ["BOT_SPEECH_OVERLAP", "1.5"],
    ["BOT_SPEECH_OVERLAP", '0.3";alert(1);"'],
  ])("rejects %s=%s naming the variable", (name, value) => {
    const r = resolveSpeechPlan({ [name]: value });
    expect(r.kind).toBe("invalid");
    expect(r.kind === "invalid" && r.message).toContain(name);
  });
});

describe("speech knobs in the fleet manifests (issue 3090)", () => {
  const read = (...p: string[]) => readFileSync(resolve(BOTS_APP, ...p), "utf8");

  it("the entrypoint documents exactly the env names speech-plan.ts reads", () => {
    const named = new Set(
      [...read("docker-entrypoint.sh").matchAll(/BOT_SPEECH_\w+/g)].map((m) => m[0]),
    );
    expect([...named].sort()).toEqual([...SPEECH_ENV_NAMES].sort());
  });

  it("the StatefulSet names only env vars the code reads, and any it sets resolve", () => {
    const manifest = read("k8s", "statefulset.yaml");
    const named = new Set([...manifest.matchAll(/BOT_SPEECH_\w+/g)].map((m) => m[0]));
    expect([...named].sort()).toEqual([...SPEECH_ENV_NAMES].sort());
    const doc = parseYaml(manifest) as {
      spec: { template: { spec: { containers: { env?: { name: string; value?: string }[] }[] } } };
    };
    const live = Object.fromEntries(
      doc.spec.template.spec.containers
        .flatMap((c) => c.env ?? [])
        .filter((e) => e.name.startsWith("BOT_SPEECH_"))
        .map((e) => [e.name, e.value]),
    );
    expect(resolveSpeechPlan(live).kind).toBe("ok");
  });
});
