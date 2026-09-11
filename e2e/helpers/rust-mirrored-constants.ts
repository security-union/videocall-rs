/**
 * Numeric constants Playwright specs mirror from Rust source. A drifted mirror
 * reds no spec, it silently weakens every assertion derived from it, so
 * `rust-mirrored-constants.test.ts` locks every `RUST_MIRRORS` entry to its Rust
 * symbol, and fails on any numeric export of this module absent from it.
 */

/** Peer heartbeat keepalive; `GLOW_DEADMAN_MS` is 2.5x it (`peer_tile.rs`). */
export const HEARTBEAT_KEEPALIVE_INTERVAL_MS = 5000;

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

/** Self-view signal meter thresholds (`connection_quality_indicator.rs`). They
 * drive the ring colour as well as the specs, so a drifted mirror would weaken
 * both at once. */
export const CQI = {
  WARN_THRESHOLD_MS: 300,
  CRITICAL_THRESHOLD_MS: 500,
  ENTER_COUNT: 3,
  EXIT_COUNT: 5,
  SAMPLE_GAP_RESET_MS: 10_000,
} as const;

/** Drawer drag clamp bounds and chat's fixed width (`attendants_layout.rs`).
 * The floor was mirrored as a stale 240 in `drawer-resize.spec.ts` for as long
 * as the Rust value has been 300, which is the drift this lock exists to catch. */
export const DRAWER = {
  DRAWER_MIN_WIDTH: 300,
  DRAWER_MAX_ABS: 720,
  CHAT_DRAWER_WIDTH: 360,
  /** Tile band `max_total_reserve` keeps: it caps a lone drawer below 800px. */
  MIN_GRID_BAND: 400,
} as const;

export const RUST_MIRRORS: Record<string, Record<string, number>> = {
  "videocall-aq/src/constants.rs": { HEARTBEAT_KEEPALIVE_INTERVAL_MS },
  "videocall-codecs/src/jitter_buffer.rs": {
    MAX_PLAYOUT_AGE_MS,
    MAX_KEYFRAME_LESS_HOLD_MS,
  },
  "dioxus-ui/src/components/decode_budget.rs": { ...BUDGET },
  "dioxus-ui/src/components/density.rs": { ...DENSITY },
  "dioxus-ui/src/components/connection_quality_indicator.rs": { ...CQI },
  "dioxus-ui/src/components/attendants_layout.rs": { ...DRAWER },
};
