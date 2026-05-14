import { useCallback, useState } from "react";
import * as Toast from "@radix-ui/react-toast";
import { CheckCircle2, AlertCircle, Info as InfoIcon, X } from "lucide-react";

export type ToastVariant = "success" | "error" | "info";

export interface ToastEntry {
  id: number;
  title: string;
  description?: string;
  variant: ToastVariant;
}

let toastIdCounter = 0;

export function useToastShelf() {
  const [entries, setEntries] = useState<ToastEntry[]>([]);
  const push = useCallback((t: Omit<ToastEntry, "id">) => {
    toastIdCounter += 1;
    const id = toastIdCounter;
    setEntries((prev) => [...prev, { id, ...t }]);
  }, []);
  const dismiss = useCallback((id: number) => {
    setEntries((prev) => prev.filter((e) => e.id !== id));
  }, []);
  return { entries, push, dismiss };
}

interface ToastShelfProps {
  entries: ToastEntry[];
  onDismiss: (id: number) => void;
}

const VARIANT_STYLES: Record<ToastVariant, string> = {
  success: "border-emerald-200 bg-emerald-50 text-emerald-900",
  error: "border-red-200 bg-red-50 text-red-900",
  info: "border-sky-200 bg-sky-50 text-sky-900",
};

export function ToastShelf({ entries, onDismiss }: ToastShelfProps) {
  return (
    <>
      {entries.map((t) => (
        <Toast.Root
          key={t.id}
          open
          onOpenChange={(open) => {
            if (!open) onDismiss(t.id);
          }}
          className={`flex items-start gap-3 rounded-lg border p-3 shadow-md ${VARIANT_STYLES[t.variant]}`}
        >
          <div className="mt-0.5 shrink-0">
            {t.variant === "success" && <CheckCircle2 className="h-5 w-5" />}
            {t.variant === "error" && <AlertCircle className="h-5 w-5" />}
            {t.variant === "info" && <InfoIcon className="h-5 w-5" />}
          </div>
          <div className="min-w-0 flex-1">
            <Toast.Title className="text-sm font-semibold">{t.title}</Toast.Title>
            {t.description && (
              <Toast.Description className="mt-0.5 text-xs">{t.description}</Toast.Description>
            )}
          </div>
          <Toast.Close
            aria-label="Dismiss"
            className="shrink-0 rounded p-1 text-neutral-500 hover:bg-white/50"
          >
            <X className="h-4 w-4" />
          </Toast.Close>
        </Toast.Root>
      ))}
    </>
  );
}
