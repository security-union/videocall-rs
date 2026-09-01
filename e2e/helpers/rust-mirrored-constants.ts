/**
 * Numeric constants Playwright specs mirror from Rust source. A drifted mirror
 * reds no spec, it silently weakens every assertion derived from it, so
 * `rust-mirrored-constants.test.ts` locks every `RUST_MIRRORS` entry to its Rust
 * symbol, and fails on any numeric export of this module absent from it.
 */

export const MAX_PLAYOUT_AGE_MS = 1800;
export const MAX_KEYFRAME_LESS_HOLD_MS = 6000;

export const BUDGET = {
  FPS_STEP_DOWN: 24,
  FPS_STEP_UP: 30,
  FPS_SEVERE: 12,
  LONGTASK_SEVERE_MS_PER_SEC: 700,
  SUSTAIN_SAMPLES: 3,
  RECOVERY_HOLD: 5,
  STEP_DOWN_COOLDOWN_MS: 2000,
  STEP_UP_COOLDOWN_MS: 4000,
  MIN_CAP: 1,
} as const;

/** Geometry the viewport-filter spec's `+N` window depends on (`density.rs`). */
export const DENSITY = {
  MOBILE_WIDTH_BREAKPOINT_PX: 568,
  STANDARD_MIN_TILE_WIDTH_DESKTOP_PX: 340,
} as const;

export const RUST_MIRRORS: Record<string, Record<string, number>> = {
  "videocall-codecs/src/jitter_buffer.rs": {
    MAX_PLAYOUT_AGE_MS,
    MAX_KEYFRAME_LESS_HOLD_MS,
  },
  "dioxus-ui/src/components/decode_budget.rs": { ...BUDGET },
  "dioxus-ui/src/components/density.rs": { ...DENSITY },
};
