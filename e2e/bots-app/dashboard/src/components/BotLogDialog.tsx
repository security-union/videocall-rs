import { useEffect, useRef, useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { BotSnapshot } from "../api/types";

interface BotLogDialogProps {
  /** The bot whose log to display, or `null` to keep the dialog closed. */
  bot: BotSnapshot | null;
  onClose: () => void;
}

/**
 * Per-bot rolling log viewer. Polls `GET /api/bots/:id/log?since=<n>`
 * every 2.5s while open and appends new lines to the buffer. For
 * SSH-hosted bots this surfaces the SSH ChildProcess's stdout/stderr;
 * for local Playwright bots the buffer is currently empty (those bots
 * log to the orchestrator's stdout, not into the registry — that's a
 * future enhancement). Either way, the dialog still opens and the
 * "no log entries yet" copy makes the state legible.
 */
export function BotLogDialog({ bot, onClose }: BotLogDialogProps) {
  const open = bot !== null;
  const [lines, setLines] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const cursorRef = useRef<number>(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // Reset the cursor and buffer every time the dialog opens for a
  // different bot. We deliberately do this inside an effect (not at
  // render time) so React's commit can flush the buffer-reset before
  // the next poll fires.
  useEffect(() => {
    if (!open) return;
    cursorRef.current = 0;
    setLines([]);
    setError(null);
  }, [open, bot?.botId]);

  // Polling loop. Only active while `open && bot !== null`.
  useEffect(() => {
    if (!open || !bot) return;
    let cancelled = false;
    const tick = async (): Promise<void> => {
      try {
        const res = await api.botLog(bot.botId, cursorRef.current);
        if (cancelled) return;
        if (res.lines.length > 0) {
          setLines((prev) => prev.concat(res.lines));
        }
        cursorRef.current = res.totalLines;
        setError(null);
      } catch (e) {
        if (cancelled) return;
        const msg = e instanceof DashboardApiError ? e.message : (e as Error).message;
        setError(msg);
      }
    };
    void tick();
    const interval = setInterval(tick, 2_500);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [open, bot]);

  // Auto-scroll to the bottom every time new lines arrive. We use a
  // requestAnimationFrame indirection because Radix's scroll
  // container hasn't been laid out yet on the initial open.
  useEffect(() => {
    if (!scrollRef.current) return;
    const el = scrollRef.current;
    requestAnimationFrame(() => {
      el.scrollTop = el.scrollHeight;
    });
  }, [lines.length]);

  return (
    <Dialog.Root open={open} onOpenChange={(o) => !o && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(95vw,720px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-4 shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800"
          data-testid="bot-log-dialog"
        >
          <div className="flex items-start justify-between">
            <div>
              <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
                Bot logs
              </Dialog.Title>
              <Dialog.Description className="mt-1 text-xs text-neutral-500 dark:text-slate-400">
                {bot ? (
                  <>
                    Polling{" "}
                    <code className="font-mono text-[11px]">{`/api/bots/${bot.botId}/log`}</code>{" "}
                    every 2.5s.
                  </>
                ) : null}
              </Dialog.Description>
            </div>
            <Dialog.Close className="rounded p-1 text-neutral-400 hover:bg-neutral-100 dark:text-slate-500 dark:hover:bg-slate-700">
              <X className="h-4 w-4" />
            </Dialog.Close>
          </div>

          {error && (
            <p
              className="mt-3 rounded-md border border-red-200 bg-red-50 px-3 py-2 text-xs text-red-700 dark:border-red-800 dark:bg-red-900/30 dark:text-red-200"
              role="alert"
              data-testid="bot-log-error"
            >
              {error}
            </p>
          )}

          <div
            ref={scrollRef}
            className="mt-3 max-h-[60vh] min-h-[180px] overflow-y-auto rounded-md border border-neutral-200 bg-neutral-950 p-3 font-mono text-[11px] text-neutral-100 dark:border-slate-700"
            data-testid="bot-log-content"
          >
            {lines.length === 0 ? (
              <p className="text-neutral-400">
                No log entries yet. Local bots stream to the orchestrator&apos;s stdout; remote
                bots stream over SSH and may take a few seconds to populate.
              </p>
            ) : (
              lines.map((line, i) => (
                <div key={i} className="whitespace-pre-wrap break-words">
                  {line}
                </div>
              ))
            )}
          </div>
          <div className="mt-3 flex justify-between text-xs text-neutral-500 dark:text-slate-400">
            <span data-testid="bot-log-line-count">{lines.length} lines</span>
            <Dialog.Close asChild>
              <button
                type="button"
                className="rounded-md border border-neutral-300 px-3 py-1 text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
              >
                Close
              </button>
            </Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
