import { useState } from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { History, X } from "lucide-react";

import {
  clearLaunchedBotHistory,
  loadLaunchedBotHistory,
  removeLaunchedBot,
  type LaunchedBotHistoryEntry,
} from "../lib/botHistory";
import { ConfirmDialog } from "./ConfirmDialog";

interface LoadPreviousButtonProps {
  /**
   * Called when the operator clicks one of the history rows. The
   * parent (LaunchForm or MultiLaunchForm) is responsible for piping
   * the snapshot back into its form state — the button doesn't know
   * how the spec shape maps to the form's local state.
   */
  onSelect: (entry: LaunchedBotHistoryEntry) => void;
  /**
   * True when a launch is in flight. While set, the button stays
   * visible but disabled to mirror the Reset button's behavior.
   */
  disabled?: boolean;
  /**
   * Override for tests: when omitted, the component reads from
   * localStorage via {@link loadLaunchedBotHistory}.
   */
  testId?: string;
}

/**
 * Format a short, locale-friendly weekday + HH:MM stamp for a history
 * row. Example: `"Sun 10:42"`. We deliberately drop seconds and the
 * date — the operator usually cares "recent or not recent", and the
 * weekday gives that without the row turning into a long line.
 */
function formatStamp(launchedAt: number): string {
  const d = new Date(launchedAt);
  const weekday = d.toLocaleDateString(undefined, { weekday: "short" });
  const hh = d.getHours().toString().padStart(2, "0");
  const mm = d.getMinutes().toString().padStart(2, "0");
  return `${weekday} ${hh}:${mm}`;
}

/**
 * Standalone "Load previous" button that sits next to Reset/Launch in
 * the LaunchForm + MultiLaunchForm action rows. Clicking the button
 * opens a Radix DropdownMenu showing the most-recent bot launches (up
 * to {@link MAX_ENTRIES}), each as a single row the operator can
 * click to repopulate the form. Per-row remove and a footer
 * "Clear history" let the operator manage the list.
 *
 * The button is always rendered (so its position next to Reset/Launch
 * is stable across renders); when the history is empty the dropdown
 * shows a friendly empty-state hint instead of an item list.
 */
export function LoadPreviousButton({
  onSelect,
  disabled,
  testId = "load-previous-button",
}: LoadPreviousButtonProps) {
  // Re-read on every menu open so cross-tab launches show up without a
  // page reload. Cheap — the worst case is parsing a 20-entry JSON
  // blob — and avoids the complexity of subscribing to `storage`.
  const [entries, setEntries] = useState<LaunchedBotHistoryEntry[]>(() =>
    loadLaunchedBotHistory(),
  );
  const [confirmClearOpen, setConfirmClearOpen] = useState(false);

  const refresh = () => setEntries(loadLaunchedBotHistory());

  const handleRemove = (entry: LaunchedBotHistoryEntry, e: React.MouseEvent) => {
    // Prevent the DropdownMenu.Item from also firing onSelect — without
    // this stopPropagation the click on the "×" would both remove the
    // row AND fire a row-select on the same press.
    e.preventDefault();
    e.stopPropagation();
    removeLaunchedBot(entry.launchedAt);
    refresh();
  };

  const handleClearAll = () => {
    clearLaunchedBotHistory();
    setEntries([]);
    setConfirmClearOpen(false);
  };

  return (
    <>
      <DropdownMenu.Root onOpenChange={(open) => open && refresh()}>
        <DropdownMenu.Trigger asChild>
          <button
            type="button"
            disabled={disabled}
            data-testid={testId}
            aria-label="Load previously launched bot"
            className="inline-flex items-center gap-2 rounded-lg border border-neutral-300 bg-white px-4 py-2 text-sm font-medium text-neutral-700 shadow-sm transition-colors hover:bg-neutral-50 disabled:cursor-not-allowed disabled:bg-neutral-100 disabled:text-neutral-400 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700 dark:disabled:bg-slate-900 dark:disabled:text-slate-500"
          >
            <History className="h-4 w-4" />
            Load previous
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content
            align="end"
            sideOffset={6}
            className="z-50 max-h-[60vh] min-w-[24rem] overflow-y-auto rounded-lg border border-neutral-200 bg-white p-1 shadow-lg dark:border-slate-700 dark:bg-slate-800"
            data-testid={`${testId}-content`}
          >
            {entries.length === 0 ? (
              <div
                className="px-3 py-4 text-xs text-neutral-500 dark:text-slate-400"
                data-testid={`${testId}-empty`}
              >
                No previous launches yet. Launch a bot, and it&apos;ll appear here next
                time.
              </div>
            ) : (
              <>
                {entries.map((entry) => (
                  <DropdownMenu.Item
                    key={entry.launchedAt}
                    onSelect={(ev) => {
                      ev.preventDefault();
                      onSelect(entry);
                    }}
                    data-testid={`${testId}-entry-${entry.launchedAt}`}
                    className="group flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-xs text-neutral-700 outline-none data-[highlighted]:bg-sky-50 data-[highlighted]:text-sky-700 dark:text-slate-200 dark:data-[highlighted]:bg-sky-900/40 dark:data-[highlighted]:text-sky-200"
                  >
                    <span className="text-neutral-500 dark:text-slate-400">
                      [{formatStamp(entry.launchedAt)}]
                    </span>
                    <span className="font-medium">{entry.participant}</span>
                    <span className="text-neutral-400 dark:text-slate-500">·</span>
                    <span
                      className="truncate font-mono text-[11px] text-neutral-600 dark:text-slate-300"
                      title={entry.meetingURL}
                    >
                      {entry.meetingURL}
                    </span>
                    <span className="text-neutral-400 dark:text-slate-500">·</span>
                    <span className="font-mono text-[11px] text-neutral-500 dark:text-slate-400">
                      {entry.runLocationLabel}
                    </span>
                    <button
                      type="button"
                      onClick={(e) => handleRemove(entry, e)}
                      className="ml-auto rounded p-0.5 text-neutral-400 opacity-0 transition-opacity hover:bg-neutral-100 hover:text-red-600 group-hover:opacity-100 group-data-[highlighted]:opacity-100 dark:text-slate-500 dark:hover:bg-slate-700 dark:hover:text-red-400"
                      aria-label={`Remove ${entry.participant} from history`}
                      data-testid={`${testId}-remove-${entry.launchedAt}`}
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </DropdownMenu.Item>
                ))}
                <DropdownMenu.Separator className="my-1 h-px bg-neutral-200 dark:bg-slate-700" />
                <DropdownMenu.Item
                  onSelect={(ev) => {
                    ev.preventDefault();
                    setConfirmClearOpen(true);
                  }}
                  data-testid={`${testId}-clear`}
                  className="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-xs text-red-600 outline-none data-[highlighted]:bg-red-50 dark:text-red-300 dark:data-[highlighted]:bg-red-900/30"
                >
                  Clear history
                </DropdownMenu.Item>
              </>
            )}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
      <ConfirmDialog
        open={confirmClearOpen}
        title="Clear launched-bot history?"
        body="This removes every previously-launched bot from the dropdown. It does NOT stop any running bot. Cannot be undone."
        confirmLabel="Clear history"
        destructive
        onCancel={() => setConfirmClearOpen(false)}
        onConfirm={handleClearAll}
      />
    </>
  );
}
