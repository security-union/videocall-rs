import { forwardRef, useCallback, useImperativeHandle, useRef, useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import { X } from "lucide-react";

import { useFieldHistory, type UseFieldHistory } from "../../lib/fieldHistory";

interface HistoryInputProps {
  /** Stable identifier used as the localStorage namespace. */
  fieldKey: string;
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
  className?: string;
  /** Forwarded to the underlying `<input data-testid=…>`. */
  testId?: string;
  /** ARIA invalid flag, forwarded to the input. */
  ariaInvalid?: boolean;
  /** Optional label for screen-reader announcement of the popover list. */
  ariaLabel?: string;
  type?: "text" | "url" | "email";
  /** Optional native input id for `<label htmlFor>` association. */
  id?: string;
  /** Disable the input + suppress the popover. */
  disabled?: boolean;
  /** Max number of suggestions to retain (defaults to 10). */
  maxEntries?: number;
}

export interface HistoryInputHandle {
  /** Append the current value (or a passed-in value) to history. */
  commit: (value?: string) => void;
}

/**
 * Free-text input with a Radix `Popover` suggestion list backed by
 * `useFieldHistory`. The styling matches the other launch-form inputs
 * exactly so the autocomplete addition is visually invisible until the
 * user focuses a field with history available.
 *
 * Keyboard model (when the popover is open):
 *   ArrowDown / ArrowUp — move highlight
 *   Enter               — commit the highlighted suggestion
 *   Esc                 — close the popover
 *   Tab                 — close the popover and let focus advance
 *
 * The "×" affordance on each row removes that entry from history.
 * Clicking outside or losing focus closes the popover.
 */
export const HistoryInput = forwardRef<HistoryInputHandle, HistoryInputProps>(function HistoryInput(
  {
    fieldKey,
    value,
    onChange,
    placeholder,
    className,
    testId,
    ariaInvalid,
    ariaLabel,
    type = "text",
    id,
    disabled,
    maxEntries,
  },
  ref,
) {
  const history: UseFieldHistory = useFieldHistory(fieldKey, { maxEntries });
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState<number>(-1);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useImperativeHandle(
    ref,
    (): HistoryInputHandle => ({
      commit: (v?: string) => history.push(v ?? value),
    }),
    [history, value],
  );

  const hasSuggestions = history.entries.length > 0;

  const select = useCallback(
    (entry: string) => {
      onChange(entry);
      setOpen(false);
      setHighlight(-1);
      // Return focus to the input so the user can keep typing.
      inputRef.current?.focus();
    },
    [onChange],
  );

  const handleKeyDown: React.KeyboardEventHandler<HTMLInputElement> = (e) => {
    if (!open || !hasSuggestions) {
      if (e.key === "ArrowDown" && hasSuggestions) {
        e.preventDefault();
        setOpen(true);
        setHighlight(0);
      }
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlight((h) => (h + 1) % history.entries.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlight((h) => (h - 1 + history.entries.length) % history.entries.length);
    } else if (e.key === "Enter") {
      if (highlight >= 0 && highlight < history.entries.length) {
        e.preventDefault();
        select(history.entries[highlight].value);
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      setOpen(false);
      setHighlight(-1);
    } else if (e.key === "Tab") {
      setOpen(false);
      setHighlight(-1);
    }
  };

  return (
    <Popover.Root
      open={open && hasSuggestions && !disabled}
      onOpenChange={(o) => {
        // Allow Radix to close on outside-click / Esc, but do not let
        // it auto-open: opening is driven exclusively by focus.
        if (!o) {
          setOpen(false);
          setHighlight(-1);
        }
      }}
    >
      <Popover.Anchor asChild>
        <input
          ref={inputRef}
          id={id}
          type={type}
          value={value}
          onChange={(e) => {
            onChange(e.target.value);
            // Typing should not close an already-open list — the user
            // may want to glance at history while editing.
          }}
          onFocus={() => {
            if (hasSuggestions) {
              setOpen(true);
              setHighlight(-1);
            }
          }}
          onBlur={(e) => {
            // Skip closing if focus moved into the popover content
            // (e.g. the user clicked a suggestion's "×" button).
            const next = e.relatedTarget as HTMLElement | null;
            if (next && next.closest("[data-history-popover]")) return;
            setOpen(false);
            setHighlight(-1);
          }}
          onKeyDown={handleKeyDown}
          placeholder={placeholder}
          className={className}
          data-testid={testId}
          aria-invalid={ariaInvalid ? "true" : "false"}
          aria-label={ariaLabel}
          aria-autocomplete="list"
          aria-expanded={open && hasSuggestions}
          aria-controls={open && hasSuggestions ? `${fieldKey}-history-list` : undefined}
          autoComplete="off"
          disabled={disabled}
        />
      </Popover.Anchor>
      <Popover.Portal>
        <Popover.Content
          data-history-popover
          align="start"
          sideOffset={4}
          // Don't steal focus on open — focus must stay in the input
          // so typing/arrow-keys still work.
          onOpenAutoFocus={(e) => e.preventDefault()}
          onCloseAutoFocus={(e) => e.preventDefault()}
          className="z-50 w-[var(--radix-popover-trigger-width)] overflow-y-auto rounded-lg border border-neutral-200 bg-white p-1 shadow-lg dark:border-slate-700 dark:bg-slate-800"
          style={{ maxHeight: "16rem" }}
        >
          <ul
            id={`${fieldKey}-history-list`}
            role="listbox"
            aria-label={ariaLabel ? `${ariaLabel} history` : "Field history"}
            data-testid={testId ? `${testId}-history` : undefined}
            className="flex flex-col"
          >
            {history.entries.map((entry, i) => {
              const isActive = i === highlight;
              return (
                <li
                  key={entry.value}
                  role="option"
                  aria-selected={isActive}
                  data-highlighted={isActive ? "" : undefined}
                  className={`flex items-center gap-2 rounded-md px-2 py-1.5 text-sm ${
                    isActive
                      ? "bg-sky-50 text-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
                      : "text-neutral-700 hover:bg-neutral-50 dark:text-slate-200 dark:hover:bg-slate-700"
                  }`}
                  // Mousedown fires before blur — use it so the input's
                  // onBlur doesn't slam the popover shut before the
                  // click registers on the row.
                  onMouseDown={(e) => {
                    e.preventDefault();
                    select(entry.value);
                  }}
                  onMouseEnter={() => setHighlight(i)}
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-xs">{entry.value}</span>
                  <button
                    type="button"
                    onMouseDown={(e) => {
                      // Stop the row's mousedown handler from firing
                      // (which would select the entry); we only want
                      // to delete it here.
                      e.preventDefault();
                      e.stopPropagation();
                      history.remove(entry.value);
                    }}
                    aria-label={`Remove “${entry.value}” from history`}
                    className="shrink-0 rounded p-0.5 text-neutral-400 hover:bg-neutral-200 hover:text-neutral-700 dark:text-slate-400 dark:hover:bg-slate-600 dark:hover:text-slate-100"
                  >
                    <X className="h-3 w-3" />
                  </button>
                </li>
              );
            })}
          </ul>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
});
