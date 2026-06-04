/**
 * Launched-bot history backed by `localStorage`.
 *
 * Every successful Launch in the dashboard (single or multi) snapshots
 * the LaunchForm spec at the moment of submit into a small ring buffer
 * persisted under one `localStorage` key. The Load-previous button in
 * the Launch + MultiLaunch forms then surfaces those snapshots as a
 * dropdown so the operator can pre-fill the form with a previously
 * launched bot's exact settings — useful for re-spawning the same bot
 * after a session, or for quickly cloning a known-good config across
 * many meetings.
 *
 * Storage shape (one entry per launch, newest-first when loaded):
 *   localStorage["bots-app-dashboard:launched-bot-history"] = JSON.stringify([
 *     { spec: {...}, launchedAt: 1730000000000,
 *       meetingURL: "...", participant: "alice",
 *       runLocationLabel: "local" },
 *     ...
 *   ])
 *
 * Invariants enforced by the helpers below:
 *   - De-duplicated by full-spec equality (re-launching an identical
 *     spec bumps the existing entry's `launchedAt` rather than adding
 *     a duplicate row).
 *   - Capped at {@link MAX_ENTRIES} (oldest entries are dropped).
 *   - Sorted by `launchedAt` DESC (most recent first) on load.
 *   - Corrupt JSON / Safari private-mode storage exceptions return
 *     an empty list rather than throwing.
 */

import type { LaunchFormInitial } from "../components/LaunchForm";

export interface LaunchedBotHistoryEntry {
  /**
   * Snapshot of the entire LaunchForm spec at the moment of submit.
   * Identical shape to {@link LaunchFormInitial} so it can be passed
   * verbatim back into `<LaunchForm initialValues={...} />`.
   */
  spec: LaunchFormInitial;
  /** ms epoch when this launch was submitted. */
  launchedAt: number;
  /**
   * Cached meeting URL — duplicated from `spec.meetingURL` so the
   * dropdown row can render without re-reading the nested spec.
   */
  meetingURL: string;
  /**
   * Cached participant handle — duplicated from `spec.participant` for
   * the same reason as {@link meetingURL}.
   */
  participant: string;
  /**
   * Display label of the run location: either the literal string
   * `"local"` or `"ssh:<hostLabel>"`. Cached at write-time so the
   * dropdown row doesn't need to re-derive it.
   */
  runLocationLabel: string;
}

export const STORAGE_KEY = "bots-app-dashboard:launched-bot-history";
export const MAX_ENTRIES = 20;

/**
 * Type-guard for an unknown value being a {@link LaunchedBotHistoryEntry}.
 * Used by {@link loadLaunchedBotHistory} to silently drop entries that
 * fail the structural check rather than throwing on a single bad row.
 */
function isHistoryEntry(value: unknown): value is LaunchedBotHistoryEntry {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  if (typeof v.launchedAt !== "number") return false;
  if (typeof v.meetingURL !== "string") return false;
  if (typeof v.participant !== "string") return false;
  if (typeof v.runLocationLabel !== "string") return false;
  if (typeof v.spec !== "object" || v.spec === null) return false;
  return true;
}

/**
 * Pure full-spec equality check used to de-dupe writes. Two entries
 * collapse to one when every shallow LaunchFormInitial field matches —
 * since the spec carries only primitives and a couple of string-typed
 * enums, JSON.stringify is the cheapest reliable comparison.
 */
function specsEqual(a: LaunchFormInitial, b: LaunchFormInitial): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Read the saved history from localStorage and return entries newest
 * first. Returns `[]` if storage is empty, unavailable (SSR), or
 * holds malformed JSON.
 */
export function loadLaunchedBotHistory(): LaunchedBotHistoryEntry[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    const filtered = parsed.filter(isHistoryEntry);
    // Defensive re-sort — most writers go through `recordLaunchedBot`
    // which preserves DESC order, but adversarial localStorage edits
    // (or older bug fixes) could leave entries out of order.
    filtered.sort((a, b) => b.launchedAt - a.launchedAt);
    return filtered;
  } catch {
    // Malformed JSON, storage disabled, or quota exception — treat as
    // empty so the dashboard keeps working.
    return [];
  }
}

/**
 * Append a new launch to the history (or bump an existing matching
 * entry's `launchedAt`). The list is then re-sorted DESC and capped at
 * {@link MAX_ENTRIES}. Silently no-ops if localStorage is unavailable.
 */
export function recordLaunchedBot(entry: LaunchedBotHistoryEntry): void {
  if (typeof window === "undefined") return;
  const existing = loadLaunchedBotHistory();
  // Drop any prior entry whose spec matches the incoming one — the new
  // entry's timestamp wins. This keeps the dropdown from filling up
  // with twenty rows that are all the same config.
  const withoutDup = existing.filter((e) => !specsEqual(e.spec, entry.spec));
  const next = [entry, ...withoutDup];
  next.sort((a, b) => b.launchedAt - a.launchedAt);
  const capped = next.slice(0, MAX_ENTRIES);
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(capped));
  } catch {
    // Quota exceeded / private-mode Safari — silently swallow. The
    // dashboard keeps working without the history; the operator just
    // doesn't see this launch appear on the dropdown next session.
  }
}

/**
 * Drop the history entry whose `launchedAt` timestamp matches. Used by
 * the per-row "×" remove control on the dropdown.
 */
export function removeLaunchedBot(launchedAt: number): void {
  if (typeof window === "undefined") return;
  const existing = loadLaunchedBotHistory();
  const next = existing.filter((e) => e.launchedAt !== launchedAt);
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // see recordLaunchedBot — silent.
  }
}

/**
 * Wipe every entry. Used by the "Clear history" footer action.
 */
export function clearLaunchedBotHistory(): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    // see recordLaunchedBot — silent.
  }
}

/**
 * Build the canonical run-location display label for a given spec.
 * Exported so the LaunchForm wiring can compute the value the same way
 * the loaders / tests do.
 */
export function runLocationLabelFor(spec: LaunchFormInitial): string {
  if (spec.runLocation === "ssh") {
    const host = spec.sshHostLabel.trim();
    return host ? `ssh:${host}` : "ssh";
  }
  return "local";
}
