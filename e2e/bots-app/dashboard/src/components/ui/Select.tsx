import * as RxSelect from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";

interface SelectOption {
  value: string;
  label: string;
}

interface SelectProps {
  value: string;
  onValueChange: (v: string) => void;
  options: SelectOption[];
  placeholder?: string;
  testId?: string;
  disabled?: boolean;
}

/**
 * Thin styled wrapper over Radix's Select. Centralized here so the
 * many pick-lists across the form (network, costume, audio, …) all
 * look identical and so we can revisit visual polish in one place.
 */
export function Select({ value, onValueChange, options, placeholder, testId, disabled }: SelectProps) {
  return (
    <RxSelect.Root value={value} onValueChange={onValueChange} disabled={disabled}>
      <RxSelect.Trigger
        className="inline-flex w-full items-center justify-between gap-2 rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 disabled:cursor-not-allowed disabled:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-100 dark:focus:border-sky-400 dark:focus:ring-sky-400 dark:disabled:bg-slate-900"
        aria-label={placeholder ?? "Select"}
        data-testid={testId}
      >
        <RxSelect.Value placeholder={placeholder ?? "Choose…"} />
        <RxSelect.Icon>
          <ChevronDown className="h-4 w-4 text-neutral-400 dark:text-slate-400" />
        </RxSelect.Icon>
      </RxSelect.Trigger>
      <RxSelect.Portal>
        <RxSelect.Content
          position="popper"
          sideOffset={4}
          className="z-50 max-h-72 overflow-y-auto rounded-lg border border-neutral-200 bg-white shadow-lg dark:border-slate-700 dark:bg-slate-800"
        >
          <RxSelect.Viewport className="p-1">
            {options.map((opt) => (
              <RxSelect.Item
                key={opt.value}
                value={opt.value}
                className="relative flex cursor-pointer select-none items-center gap-2 rounded-md px-2 py-1.5 text-sm text-neutral-700 outline-none data-[highlighted]:bg-sky-50 data-[highlighted]:text-sky-700 dark:text-slate-200 dark:data-[highlighted]:bg-sky-900/40 dark:data-[highlighted]:text-sky-200"
              >
                <RxSelect.ItemIndicator>
                  <Check className="h-4 w-4 text-sky-500 dark:text-sky-400" />
                </RxSelect.ItemIndicator>
                <RxSelect.ItemText>{opt.label}</RxSelect.ItemText>
              </RxSelect.Item>
            ))}
          </RxSelect.Viewport>
        </RxSelect.Content>
      </RxSelect.Portal>
    </RxSelect.Root>
  );
}
