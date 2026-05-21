import { useId, useState, type ReactNode } from "react";
import * as Popover from "@radix-ui/react-popover";
import { HelpCircle } from "lucide-react";

interface HelpPopoverProps {
  /** Short label naming the field this help belongs to. Used for ARIA. */
  fieldLabel: string;
  /** Markdown-like JSX body shown inside the popover. */
  children: ReactNode;
  /** Optional test-id forwarded to the trigger button. */
  testId?: string;
}

/**
 * Small `(?)` info button beside a field label that opens a help
 * popover. Triggers on click, focus, and hover-to-open for desktop
 * convenience; closes on outside-click, blur, or Esc. The trigger is
 * 16×16, accessible, and the panel is positioned by Radix Popover.
 *
 * Hover model: mousing onto the trigger pre-opens the popover after a
 * short delay (matching native tooltip behavior on desktop) without
 * stealing focus. Clicking pins the popover open until the user
 * dismisses it — important for content the operator wants to read in
 * full while typing in the field.
 */
export function HelpPopover({ fieldLabel, children, testId }: HelpPopoverProps) {
  const [open, setOpen] = useState(false);
  // We treat hover-open as "transient": closing on mouseleave only
  // when the popover wasn't clicked-open. `pinned` flips to true once
  // the user activates by click/keyboard.
  const [pinned, setPinned] = useState(false);
  const labelId = useId();

  const openTransient = (): void => {
    if (!pinned) setOpen(true);
  };
  const closeTransient = (): void => {
    if (!pinned) setOpen(false);
  };

  return (
    <Popover.Root
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (!o) setPinned(false);
      }}
    >
      <Popover.Trigger asChild>
        <button
          type="button"
          aria-label={`Help for ${fieldLabel}`}
          aria-describedby={open ? labelId : undefined}
          onMouseEnter={openTransient}
          onMouseLeave={closeTransient}
          onFocus={openTransient}
          onBlur={closeTransient}
          onClick={() => {
            setPinned((p) => !p);
            setOpen(true);
          }}
          className="inline-flex h-4 w-4 items-center justify-center rounded-full text-neutral-400 hover:text-sky-600 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:text-slate-500 dark:hover:text-sky-300"
          data-testid={testId}
        >
          <HelpCircle className="h-3.5 w-3.5" />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          id={labelId}
          side="top"
          align="start"
          sideOffset={6}
          collisionPadding={8}
          onMouseEnter={openTransient}
          onMouseLeave={closeTransient}
          onOpenAutoFocus={(e) => e.preventDefault()}
          className="z-50 max-w-sm rounded-lg border border-neutral-200 bg-white p-3 text-xs text-neutral-700 shadow-lg dark:border-slate-700 dark:bg-slate-800 dark:text-slate-200"
          data-testid={testId ? `${testId}-content` : undefined}
        >
          {children}
          <Popover.Arrow className="fill-white dark:fill-slate-800" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
