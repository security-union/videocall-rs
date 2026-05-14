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
 * group renders one option per entry. `"none"` is the guest-join
 * path — no session cookie injected; works only on meetings that
 * allow guests to land on `/meeting/<id>` without auth.
 */
export const AUTH_BACKENDS = [
  { value: "jwt", label: "JWT (cookie injection)" },
  { value: "storage-state", label: "Storage-state (replay OAuth)" },
  { value: "none", label: "Guest (no auth)" },
] as const;

export type AuthBackend = (typeof AUTH_BACKENDS)[number]["value"];

/**
 * Network preset metadata surfaced by the Help page. Each entry
 * mirrors a `videocall-netsim` preset and carries the
 * characteristic numbers a user needs to pick the right profile.
 * Kept in lockstep with the Rust crate's `profiles.rs` — drift
 * surfaces as a typecheck error in `Help.test.tsx`.
 */
export interface NetsimPresetMeta {
  name: NetsimPreset;
  description: string;
  /** Round-trip-ish latency in milliseconds (one-way half). */
  latencyMs: string;
  /** Packet-loss percentage as a readable string. */
  loss: string;
  /** Downstream bandwidth. */
  bandwidth: string;
}

export const NETSIM_PRESET_META: readonly NetsimPresetMeta[] = [
  {
    name: "none",
    description: "Disable the netsim shim entirely — Chrome talks to the server at full speed.",
    latencyMs: "0",
    loss: "0%",
    bandwidth: "unbounded",
  },
  {
    name: "good_wifi",
    description: "Quiet home or office WiFi; the baseline most users see on a desktop.",
    latencyMs: "20",
    loss: "0.1%",
    bandwidth: "50 Mbps",
  },
  {
    name: "good_4g",
    description: "Healthy LTE / 4G connection on a phone with decent signal.",
    latencyMs: "60",
    loss: "0.5%",
    bandwidth: "20 Mbps",
  },
  {
    name: "congested_wifi",
    description: "Crowded WiFi (coffee shop, conference) — bursts of jitter, occasional drops.",
    latencyMs: "120",
    loss: "2%",
    bandwidth: "5 Mbps",
  },
  {
    name: "lossy_mobile",
    description: "Marginal mobile signal — high latency, noticeable packet loss.",
    latencyMs: "200",
    loss: "5%",
    bandwidth: "1.5 Mbps",
  },
  {
    name: "satellite",
    description: "GEO satellite link — usable bandwidth but ~600ms latency.",
    latencyMs: "600",
    loss: "1%",
    bandwidth: "10 Mbps",
  },
  {
    name: "dialup",
    description: "Worst-case fallback: ancient narrow-band link. Stress-test the FEC path.",
    latencyMs: "150",
    loss: "3%",
    bandwidth: "56 kbps",
  },
];

/**
 * Where the bot instance runs. Two slots are wired today —
 * `"local"` and `"ssh"`. The remaining slots render as disabled with
 * a tooltip pointing at the tracking discussion.
 *
 * SSH availability ALSO depends on whether the operator has at least
 * one host registered in the registry — that runtime gating lives in
 * the LaunchForm itself, not on this static list.
 */
export const RUN_LOCATIONS = [
  { value: "local", label: "Local machine", available: true },
  { value: "future-vm", label: "Cloud VM (coming soon)", available: false },
  { value: "ssh", label: "SSH-able host", available: true },
  { value: "future-docker", label: "Docker container (coming soon)", available: false },
] as const;

export type RunLocation = (typeof RUN_LOCATIONS)[number]["value"];

/**
 * Friendly chip colors per BotStatus. Tailwind classes are baked in
 * here rather than computed in the render so the bundler can
 * tree-shake unused color combinations correctly.
 */
export const STATUS_BADGE_CLASS: Record<string, string> = {
  launching:
    "bg-amber-100 text-amber-800 border-amber-200 dark:bg-amber-900/30 dark:text-amber-300 dark:border-amber-800",
  joining:
    "bg-amber-100 text-amber-800 border-amber-200 dark:bg-amber-900/30 dark:text-amber-300 dark:border-amber-800",
  "in-meeting":
    "bg-emerald-100 text-emerald-800 border-emerald-200 dark:bg-emerald-900/30 dark:text-emerald-300 dark:border-emerald-800",
  leaving:
    "bg-orange-100 text-orange-800 border-orange-200 dark:bg-orange-900/30 dark:text-orange-300 dark:border-orange-800",
  done: "bg-neutral-100 text-neutral-600 border-neutral-200 dark:bg-slate-700 dark:text-slate-300 dark:border-slate-600",
  "done-waiting":
    "bg-sky-100 text-sky-800 border-sky-200 dark:bg-sky-900/30 dark:text-sky-300 dark:border-sky-800",
  failed:
    "bg-red-100 text-red-800 border-red-200 dark:bg-red-900/30 dark:text-red-300 dark:border-red-800",
};

/**
 * Friendly display labels per status key. Centralised so the table and
 * any future consumer (timeline, tooltips, etc.) all spell things the
 * same way. The keys here mirror `BotStatus` from the bots-app server.
 */
const STATUS_LABEL: Record<string, string> = {
  launching: "Launching",
  joining: "Joining",
  "in-meeting": "In meeting",
  leaving: "Leaving",
  done: "Done",
  failed: "Failed",
};

/**
 * Title-case a hyphenated/space-separated status key as a fallback for
 * any status the dashboard hasn't been taught about yet. Keeps unknown
 * values legible without throwing them away.
 */
function titleCase(status: string): string {
  return status
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

/**
 * Derive the badge label + key for a bot snapshot. Statuses map to
 * human-friendly labels via `STATUS_LABEL`; `done` gets a `(waiting)`
 * suffix when the bot exited because it was parked in a Waiting Room
 * (not a failure, but worth surfacing visually so operators can tell
 * apart "leave by design" from "leave because the host never admitted
 * me"). Unknown statuses fall through to a title-cased version of the
 * raw key so the UI stays informative even for new server states.
 */
export function badgeForBot(snap: {
  status: string;
  finishReason?: string;
}): { label: string; badgeKey: string } {
  if (snap.status === "done" && snap.finishReason?.startsWith("waiting-room:")) {
    return { label: "Done (waiting)", badgeKey: "done-waiting" };
  }
  const label = STATUS_LABEL[snap.status] ?? titleCase(snap.status);
  return { label, badgeKey: snap.status };
}
