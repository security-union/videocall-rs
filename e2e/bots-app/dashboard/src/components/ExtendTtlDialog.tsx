import { useEffect, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";

import type { BotSnapshot } from "../api/types";
import { isValidTtl } from "../lib/ttl";

interface ExtendTtlDialogProps {
  bot: BotSnapshot | null;
  onClose: () => void;
  onSubmit: (body: { ttl?: string; extendBy?: string }) => void;
}

export function ExtendTtlDialog({ bot, onClose, onSubmit }: ExtendTtlDialogProps) {
  const [mode, setMode] = useState<"set" | "extend">("extend");
  const [value, setValue] = useState("5m");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (bot) {
      setMode("extend");
      setValue("5m");
      setError(null);
    }
  }, [bot]);

  const submit = () => {
    if (!isValidTtl(value)) {
      setError(`"${value}" is not a valid TTL`);
      return;
    }
    if (mode === "set") {
      onSubmit({ ttl: value });
    } else {
      onSubmit({ extendBy: value });
    }
  };

  return (
    <Dialog.Root open={bot !== null} onOpenChange={(o) => !o && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content className="fixed left-1/2 top-1/2 z-50 w-[min(90vw,440px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-6 shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800">
          <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
            Extend or set TTL
          </Dialog.Title>
          <Dialog.Description className="mt-1 text-sm text-neutral-600 dark:text-slate-300">
            {bot ? (
              <>
                {bot.participant} — current TTL{" "}
                <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-xs dark:bg-slate-900 dark:text-slate-200">
                  {bot.ttl}
                </code>
              </>
            ) : null}
          </Dialog.Description>

          <div className="mt-4 flex gap-2">
            <button
              type="button"
              className={`rounded-md border px-3 py-1.5 text-sm ${
                mode === "extend"
                  ? "border-sky-300 bg-sky-50 text-sky-700 dark:border-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
                  : "border-neutral-200 bg-white text-neutral-600 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700"
              }`}
              onClick={() => setMode("extend")}
            >
              Extend by
            </button>
            <button
              type="button"
              className={`rounded-md border px-3 py-1.5 text-sm ${
                mode === "set"
                  ? "border-sky-300 bg-sky-50 text-sky-700 dark:border-sky-700 dark:bg-sky-900/40 dark:text-sky-200"
                  : "border-neutral-200 bg-white text-neutral-600 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700"
              }`}
              onClick={() => setMode("set")}
            >
              Set to
            </button>
          </div>

          <div className="mt-3 flex flex-col gap-1">
            <input
              type="text"
              value={value}
              onChange={(e) => {
                setValue(e.target.value);
                setError(null);
              }}
              className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm text-neutral-900 shadow-sm placeholder:text-neutral-400 focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-100 dark:placeholder:text-slate-500 dark:focus:border-sky-400 dark:focus:ring-sky-400"
              placeholder='"5m", "1h", "infinite"'
            />
            {error && (
              <p className="text-xs text-red-600 dark:text-red-400" role="alert">
                {error}
              </p>
            )}
          </div>

          <div className="mt-5 flex justify-end gap-2">
            <button
              type="button"
              onClick={onClose}
              className="rounded-md border border-neutral-300 bg-white px-3 py-1.5 text-sm font-medium text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={submit}
              className="rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 dark:bg-sky-500 dark:hover:bg-sky-400"
            >
              {mode === "extend" ? "Extend" : "Set"}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
