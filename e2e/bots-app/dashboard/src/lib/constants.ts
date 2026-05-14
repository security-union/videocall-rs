/**
 * Constants mirrored by hand from the Node side. Keeping these in
 * sync is enforced by the corresponding unit tests; if the Rust
 * `videocall-netsim` crate adds a profile, both lists need updating
 * and the dashboard's `network` Select picks it up automatically.
 *
 * Single source of truth on the Node side:
 * - `e2e/bots-app/src/meeting-config.ts` :: `NETSIM_PRESETS`
 */
export const NETSIM_PRESETS = [
  "none",
  "good_wifi",
  "good_4g",
  "congested_wifi",
  "lossy_mobile",
  "satellite",
  "dialup",
] as const;

export type NetsimPreset = (typeof NETSIM_PRESETS)[number];

/**
 * Suggestion chips rendered under the TTL input. Each item must
 * round-trip cleanly through `parseDurationClient` — the form
 * validates before submitting.
 */
export const TTL_SUGGESTIONS = ["5m", "30m", "1h", "infinite"] as const;

/**
 * Auth backends recognized by `bots-app run --auth`. The radio
 * group renders one option per entry.
 */
export const AUTH_BACKENDS = [
  { value: "jwt", label: "JWT (cookie injection)" },
  { value: "storage-state", label: "Storage-state (replay OAuth)" },
] as const;

export type AuthBackend = (typeof AUTH_BACKENDS)[number]["value"];

/**
 * Where the bot instance runs. Only `"local"` is wired today; the
 * other slots render as disabled with a tooltip pointing at the
 * tracking discussion.
 */
export const RUN_LOCATIONS = [
  { value: "local", label: "Local machine", available: true },
  { value: "future-vm", label: "Cloud VM (coming soon)", available: false },
  { value: "future-ssh", label: "SSH-able host (coming soon)", available: false },
  { value: "future-docker", label: "Docker container (coming soon)", available: false },
] as const;

export type RunLocation = (typeof RUN_LOCATIONS)[number]["value"];

/**
 * Friendly chip colors per BotStatus. Tailwind classes are baked in
 * here rather than computed in the render so the bundler can
 * tree-shake unused color combinations correctly.
 */
export const STATUS_BADGE_CLASS: Record<string, string> = {
  launching: "bg-amber-100 text-amber-800 border-amber-200",
  joining: "bg-amber-100 text-amber-800 border-amber-200",
  "in-meeting": "bg-emerald-100 text-emerald-800 border-emerald-200",
  leaving: "bg-orange-100 text-orange-800 border-orange-200",
  done: "bg-neutral-100 text-neutral-600 border-neutral-200",
  failed: "bg-red-100 text-red-800 border-red-200",
};
