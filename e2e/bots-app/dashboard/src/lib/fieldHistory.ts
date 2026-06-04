/**
 * Per-field input history backed by `localStorage`.
 *
 * Free-text inputs in the Launch form (meeting URL, participant,
 * display name, TTL, storage-state file path) remember what the
 * operator has previously typed and surface those values as a
 * suggestion list on focus. This keeps the dashboard feel close to a
 * native browser autofill without depending on autocomplete heuristics
 * that the OS may strip for non-standard input names.
 *
 * Storage shape, per field key:
 *   localStorage["bots-app-dashboard:history:<fieldKey>"] = JSON.stringify([
 *     { value: "alice", lastUsed: 1730000000000 },
 *     ...
 *   ])
 *
 * Invariants enforced by `useFieldHistory`:
 *   - Sorted by `lastUsed` DESC (most recent first).
 *   - De-duplicated: re-adding an existing value bumps its timestamp.
 *   - Capped at `maxEntries` (default 10) entries per field.
 *   - Empty / whitespace-only values are ignored on push.
 */

const STORAGE_PREFIX = "bots-app-dashboard:history:";
const DEFAULT_MAX_ENTRIES = 10;

export interface HistoryEntry {
  value: string;
  lastUsed: number;
}

function storageKey(fieldKey: string): string {
  return `${STORAGE_PREFIX}${fieldKey}`;
}

function safeLoad(fieldKey: string): HistoryEntry[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = window.localStorage.getItem(storageKey(fieldKey));
    if (!raw) return [];
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter(
        (e): e is HistoryEntry =>
          typeof e === "object" &&
          e !== null &&
          typeof (e as { value: unknown }).value === "string" &&
          typeof (e as { lastUsed: unknown }).lastUsed === "number",
      )
      .map((e) => ({ value: e.value, lastUsed: e.lastUsed }));
  } catch {
    // Malformed JSON or storage exception (Safari private mode) — treat as empty.
    return [];
  }
}

function safeSave(fieldKey: string, entries: HistoryEntry[]): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(storageKey(fieldKey), JSON.stringify(entries));
  } catch {
    // Storage full / disabled — silently ignore so the form keeps working.
  }
}

/**
 * Pure helper used by both the React hook below and the test suite.
 * Inserts (or refreshes) `value` in the entry list with the timestamp
 * `now`, then re-sorts and caps. Returns a new array — never mutates
 * the input.
 */
export function addEntry(
  entries: HistoryEntry[],
  value: string,
  now: number,
  maxEntries: number = DEFAULT_MAX_ENTRIES,
): HistoryEntry[] {
  const trimmed = value.trim();
  if (trimmed === "") return entries;
  const withoutDup = entries.filter((e) => e.value !== trimmed);
  const next = [{ value: trimmed, lastUsed: now }, ...withoutDup];
  // Re-sort defensively; preserved entries already carry their own
  // timestamps which should remain in DESC order, but adversarial
  // localStorage edits could violate that.
  next.sort((a, b) => b.lastUsed - a.lastUsed);
  return next.slice(0, maxEntries);
}

export function removeEntry(entries: HistoryEntry[], value: string): HistoryEntry[] {
  return entries.filter((e) => e.value !== value);
}

import { useCallback, useEffect, useState } from "react";

export interface UseFieldHistoryOptions {
  maxEntries?: number;
}

export interface UseFieldHistory {
  entries: HistoryEntry[];
  push: (value: string) => void;
  remove: (value: string) => void;
  clear: () => void;
}

/**
 * React hook wrapper. Reads from `localStorage` once on mount, then
 * keeps state in React; every `push`/`remove` writes back. The hook
 * does not subscribe to `storage` events — two dashboard tabs editing
 * the same field history at once is an acceptable last-writer-wins.
 */
export function useFieldHistory(
  fieldKey: string,
  opts: UseFieldHistoryOptions = {},
): UseFieldHistory {
  const maxEntries = opts.maxEntries ?? DEFAULT_MAX_ENTRIES;
  const [entries, setEntries] = useState<HistoryEntry[]>(() => safeLoad(fieldKey));

  // If the fieldKey changes mid-life (rare but possible if a parent
  // reuses the hook for a different identity), re-hydrate from storage.
  useEffect(() => {
    setEntries(safeLoad(fieldKey));
  }, [fieldKey]);

  const push = useCallback(
    (value: string) => {
      setEntries((prev) => {
        const next = addEntry(prev, value, Date.now(), maxEntries);
        safeSave(fieldKey, next);
        return next;
      });
    },
    [fieldKey, maxEntries],
  );

  const remove = useCallback(
    (value: string) => {
      setEntries((prev) => {
        const next = removeEntry(prev, value);
        safeSave(fieldKey, next);
        return next;
      });
    },
    [fieldKey],
  );

  const clear = useCallback(() => {
    setEntries([]);
    safeSave(fieldKey, []);
  }, [fieldKey]);

  return { entries, push, remove, clear };
}
