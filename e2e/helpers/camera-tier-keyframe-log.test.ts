import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";
import { parse as parseYaml } from "yaml";

import {
  classifyTierChangeKeyframe,
  FORCED_KEYFRAME_LOG,
  SCREEN_SHARE_COORDINATION_LOG,
  TIER_CHANGE_CAUSE,
  TIER_CHANGE_LOG,
} from "./camera-tier-keyframe-log";

const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const CAMERA_ENCODER_REL = "videocall-client/src/encode/camera_encoder.rs";
const ENCODER_STATE_REL = "videocall-client/src/encode/encoder_state.rs";
const HELPER_REL = "e2e/helpers/camera-tier-keyframe-log.ts";
const PR_CHECK_REL = ".github/workflows/pr-check-e2e-lint-hcl.yaml";

function repoSource(rel: string): string {
  const abs = resolve(REPO_ROOT, rel);
  try {
    return readFileSync(abs, "utf8");
  } catch (err) {
    throw new Error(`cannot read ${rel} (resolved ${abs}); if it moved, update ${HELPER_REL}`, {
      cause: err,
    });
  }
}

function causeLabel(src: string, variant: string): string {
  const hits = [...src.matchAll(new RegExp(`Self::${variant} => "([^"]*)"`, "g"))];
  if (hits.length !== 1) {
    throw new Error(
      `expected exactly 1 \`Self::${variant} => "..."\` arm in ${ENCODER_STATE_REL}, ` +
        `found ${hits.length}. Update ${HELPER_REL}.`,
    );
  }
  return hits[0][1];
}

describe("camera tier-change keyframe log mirrors (issue 2567)", () => {
  it("locks each console substring to its Rust log::info! format string", () => {
    const src = repoSource(CAMERA_ENCODER_REL);

    expect(src).toContain(TIER_CHANGE_LOG);
    expect(src).toContain(FORCED_KEYFRAME_LOG);
  });

  it("composes the coordination substring from the format string and its interpolated word", () => {
    const src = repoSource(CAMERA_ENCODER_REL);
    const prefix = "CameraEncoder: screen sharing ";

    // The active/inactive word is interpolated, so the line is never a source literal.
    expect(src).toContain(`"${prefix}{} `);
    expect(src).toContain('"ACTIVE"');
    expect(SCREEN_SHARE_COORDINATION_LOG).toBe(`${prefix}ACTIVE`);
  });

  it("re-runs on a change to either mirrored Rust file, or a reword lands with this lock unread", () => {
    const wf = parseYaml(repoSource(PR_CHECK_REL)) as Record<string, unknown>;
    const on = (wf.on ?? wf[String(true)]) as { pull_request?: { paths?: string[] } };
    const paths = on?.pull_request?.paths ?? [];
    for (const rel of [CAMERA_ENCODER_REL, ENCODER_STATE_REL]) {
      expect(paths, `pr-check-e2e-lint-hcl.yaml must trigger on ${rel}`).toContain(rel);
    }
  });

  it("matches the tier-change and Both cause labels but never a bare PLI", () => {
    const src = repoSource(ENCODER_STATE_REL);

    // If `Pli`'s label ever grew the phrase, a PLI repair would false-pass the spec.
    expect(causeLabel(src, "TierChange")).toContain(TIER_CHANGE_CAUSE);
    expect(causeLabel(src, "Both")).toContain(TIER_CHANGE_CAUSE);
    expect(causeLabel(src, "Pli")).not.toContain(TIER_CHANGE_CAUSE);
  });
});

describe("classifyTierChangeKeyframe", () => {
  const coordination = `[INFO] ${SCREEN_SHARE_COORDINATION_LOG} — camera tier coordination applied`;
  const tierChange = `[INFO] ${TIER_CHANGE_LOG} 'low' (640x360, 20fps, kf=100)`;
  const tierKeyframe = `[INFO] ${FORCED_KEYFRAME_LOG} 42 (${TIER_CHANGE_CAUSE})`;
  const bothKeyframe = `[INFO] ${FORCED_KEYFRAME_LOG} 42 (PLI + ${TIER_CHANGE_CAUSE})`;
  const pliKeyframe = `[INFO] ${FORCED_KEYFRAME_LOG} 42 (PLI)`;

  it("reports the full chain when the keyframe is attributed to the tier change", () => {
    expect(classifyTierChangeKeyframe([coordination, tierChange, tierKeyframe], 0)).toBe(
      "keyframe-attributed-to-tier-change",
    );
  });

  it("accepts the Both label, so a concurrent PLI still passes", () => {
    expect(classifyTierChangeKeyframe([coordination, tierChange, bothKeyframe], 0)).toBe(
      "keyframe-attributed-to-tier-change",
    );
  });

  it("stops short when only a PLI-attributed keyframe follows the tier change", () => {
    expect(classifyTierChangeKeyframe([coordination, tierChange, pliKeyframe], 0)).toBe(
      "tier-change-without-attributed-keyframe",
    );
  });

  it("stops short when no keyframe follows the tier change at all", () => {
    expect(classifyTierChangeKeyframe([coordination, tierChange], 0)).toBe(
      "tier-change-without-attributed-keyframe",
    );
  });

  it("distinguishes a coordination edge that moved no tier", () => {
    expect(classifyTierChangeKeyframe([coordination], 0)).toBe("coordination-without-tier-change");
  });

  it("reports no coordination log when the share never started", () => {
    expect(classifyTierChangeKeyframe([tierChange, tierKeyframe], 0)).toBe("no-coordination-log");
  });

  it("requires the keyframe to FOLLOW the tier change, not precede it", () => {
    expect(classifyTierChangeKeyframe([coordination, tierKeyframe, tierChange], 0)).toBe(
      "tier-change-without-attributed-keyframe",
    );
  });

  it("ignores a complete chain that predates fromIndex", () => {
    const lines = [coordination, tierChange, tierKeyframe, "[INFO] later unrelated line"];
    expect(classifyTierChangeKeyframe(lines, 3)).toBe("no-coordination-log");
  });
});
