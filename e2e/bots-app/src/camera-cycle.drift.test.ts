import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { parse as parseYaml } from "yaml";
import { describe, expect, it } from "vitest";

import { CAMERA_CYCLE_ENV_NAMES, CAMERA_CYCLE_SECS_CEILING } from "./camera-cycle";
import { CAMERA_TESTID_SELECTOR, CAMERA_TOOLTIP } from "./control-buttons";

// Drift locks for the camera duty cycle (#2362): each reads the real file.
const BOTS_APP = resolve(import.meta.dirname, "..");
const REPO_ROOT = resolve(BOTS_APP, "..", "..");
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";
const VIDEO_CONTROL_BUTTONS_REL = "dioxus-ui/src/components/video_control_buttons.rs";
const readText = (...parts: string[]): string => readFileSync(resolve(...parts), "utf8");

describe("camera-cycle drift locks", () => {
  it("the entrypoint validates exactly the env names camera-cycle.ts reads", () => {
    const sh = readText(BOTS_APP, "docker-entrypoint.sh");
    const declared = /^CAMERA_CYCLE_VARS=\(([^)]*)\)/m.exec(sh)?.[1];
    expect(declared, "docker-entrypoint.sh has no CAMERA_CYCLE_VARS array").toBeDefined();
    expect(declared!.trim().split(/\s+/)).toEqual([...CAMERA_CYCLE_ENV_NAMES]);
  });

  it("the entrypoint's phase ceiling equals CAMERA_CYCLE_SECS_CEILING", () => {
    const sh = readText(BOTS_APP, "docker-entrypoint.sh");
    const ceiling = /^CAMERA_CYCLE_SECS_CEILING=(\d+)$/m.exec(sh)?.[1];
    expect(ceiling, "docker-entrypoint.sh has no CAMERA_CYCLE_SECS_CEILING").toBeDefined();
    expect(Number(ceiling)).toBe(CAMERA_CYCLE_SECS_CEILING);
  });

  it("the StatefulSet's opt-in hint names the env vars the code actually reads", () => {
    const manifest = readText(BOTS_APP, "k8s", "statefulset.yaml");
    const named = new Set([...manifest.matchAll(/BOT_CAMERA_\w+/g)].map((m) => m[0]));
    expect([...named].sort()).toEqual([...CAMERA_CYCLE_ENV_NAMES].sort());
  });

  // A live entry here would under-represent every run (#2066).
  it("the shipped StatefulSet does not enable cycling", () => {
    const doc = parseYaml(readText(BOTS_APP, "k8s", "statefulset.yaml")) as {
      spec: { template: { spec: { containers: { env?: { name: string }[] }[] } } };
    };
    const live = doc.spec.template.spec.containers
      .flatMap((c) => c.env ?? [])
      .map((e) => e.name)
      .filter((n) => n.startsWith("BOT_CAMERA_"));
    expect(live).toEqual([]);
  });

  // Must stay scoped to CameraButton's body: a whole-file match also passes
  // when these strings move to a sibling button.
  const cameraButtonSource = (): string => {
    const rs = readText(REPO_ROOT, VIDEO_CONTROL_BUTTONS_REL);
    const m = /pub fn CameraButton\([\s\S]*?(?=\n#\[component\]|$)/.exec(rs);
    expect(m, "video_control_buttons.rs no longer defines CameraButton").not.toBeNull();
    return m![0];
  };

  it("CameraButton carries the testid the click selector is built from", () => {
    const testid = /\[data-testid="([^"]+)"\]/.exec(CAMERA_TESTID_SELECTOR)?.[1];
    expect(testid, "CAMERA_TESTID_SELECTOR is not a data-testid selector").toBeDefined();
    expect(cameraButtonSource(), `CameraButton no longer sets data-testid="${testid}"`).toContain(
      `"data-testid": "${testid}"`,
    );
  });

  it("re-runs on a video_control_buttons.rs change, or an RSX edit lands with this lock unread", () => {
    const wf = parseYaml(readText(REPO_ROOT, PR_CHECK_REL)) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    expect(
      paths,
      `pr-check-e2e-lint-hcl.yaml must trigger on ${VIDEO_CONTROL_BUTTONS_REL}`,
    ).toContain(VIDEO_CONTROL_BUTTONS_REL);
  });

  it("CameraButton's aria-label is the tooltip title the post-condition matches", () => {
    const src = cameraButtonSource();
    expect(src, "CameraButton no longer binds aria-label to tooltip_title").toContain(
      `"aria-label": tooltip_title`,
    );
    for (const tooltip of Object.values(CAMERA_TOOLTIP)) {
      expect(
        src,
        `CameraButton no longer renders "${tooltip}" — update CAMERA_TOOLTIP in control-buttons.ts`,
      ).toContain(tooltip);
    }
  });
});
