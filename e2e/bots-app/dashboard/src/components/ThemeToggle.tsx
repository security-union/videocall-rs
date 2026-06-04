import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Check, Monitor, Moon, Sun } from "lucide-react";

import { useTheme, type ThemeMode } from "../lib/theme";

interface ThemeOption {
  value: ThemeMode;
  label: string;
  icon: typeof Sun;
}

const OPTIONS: ThemeOption[] = [
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
  { value: "system", label: "System", icon: Monitor },
];

/**
 * Top-bar toggle for the light / dark / system theme picker. The
 * trigger shows the icon for the currently-selected mode (Sun/Moon/
 * Monitor) so the operator can see the active preference at a glance;
 * the dropdown lets them switch and marks the current selection with
 * a checkmark. Keyboard accessible by virtue of being a Radix
 * DropdownMenu.
 */
export function ThemeToggle() {
  const { mode, setMode } = useTheme();
  const active = OPTIONS.find((o) => o.value === mode) ?? OPTIONS[2];
  const ActiveIcon = active.icon;

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger
        className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-neutral-200 bg-white text-neutral-600 transition-colors hover:bg-neutral-50 hover:text-neutral-900 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-700 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700 dark:hover:text-slate-100"
        aria-label={`Theme: ${active.label}`}
        data-testid="theme-toggle"
      >
        <ActiveIcon className="h-4 w-4" aria-hidden="true" />
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={6}
          className="z-50 min-w-[10rem] rounded-lg border border-neutral-200 bg-white p-1 shadow-lg dark:border-slate-700 dark:bg-slate-800"
        >
          {OPTIONS.map((opt) => {
            const Icon = opt.icon;
            const selected = opt.value === mode;
            return (
              <DropdownMenu.Item
                key={opt.value}
                onSelect={() => setMode(opt.value)}
                data-testid={`theme-option-${opt.value}`}
                className="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-sm text-neutral-700 outline-none data-[highlighted]:bg-sky-50 data-[highlighted]:text-sky-700 dark:text-slate-200 dark:data-[highlighted]:bg-sky-900/40 dark:data-[highlighted]:text-sky-200"
              >
                <Icon className="h-4 w-4" aria-hidden="true" />
                <span className="flex-1">{opt.label}</span>
                {selected && (
                  <Check
                    className="h-4 w-4 text-sky-500 dark:text-sky-400"
                    aria-hidden="true"
                    data-testid={`theme-option-${opt.value}-check`}
                  />
                )}
              </DropdownMenu.Item>
            );
          })}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
