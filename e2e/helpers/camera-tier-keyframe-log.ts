// Mirrors `log::info!` formats in camera_encoder.rs and the cause labels in
// encoder_state.rs; camera-tier-keyframe-log.test.ts locks every one to that source.
export const SCREEN_SHARE_COORDINATION_LOG = "CameraEncoder: screen sharing ACTIVE";
export const TIER_CHANGE_LOG = "CameraEncoder: tier changed to";
export const FORCED_KEYFRAME_LOG = "CameraEncoder: forcing keyframe at frame";

// Substring of both the `TierChange` and `Both` labels, so a concurrent PLI matches.
export const TIER_CHANGE_CAUSE = "tier change";

export type TierChangeKeyframeStage =
  | "no-coordination-log"
  | "coordination-without-tier-change"
  | "tier-change-without-attributed-keyframe"
  | "keyframe-attributed-to-tier-change";

// `fromIndex` must be the console length captured BEFORE the share was started.
export function classifyTierChangeKeyframe(
  lines: string[],
  fromIndex: number,
): TierChangeKeyframeStage {
  const scoped = lines.slice(fromIndex);

  const coordinationAt = scoped.findIndex((l) => l.includes(SCREEN_SHARE_COORDINATION_LOG));
  if (coordinationAt === -1) {
    return "no-coordination-log";
  }

  const afterCoordination = scoped.slice(coordinationAt + 1);
  const tierChangeAt = afterCoordination.findIndex((l) => l.includes(TIER_CHANGE_LOG));
  if (tierChangeAt === -1) {
    return "coordination-without-tier-change";
  }

  const attributed = afterCoordination
    .slice(tierChangeAt + 1)
    .some((l) => l.includes(FORCED_KEYFRAME_LOG) && l.includes(TIER_CHANGE_CAUSE));

  return attributed
    ? "keyframe-attributed-to-tier-change"
    : "tier-change-without-attributed-keyframe";
}
