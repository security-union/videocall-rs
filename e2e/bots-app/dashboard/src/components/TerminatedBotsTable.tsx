import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ExternalLink, FileText, Trash2 } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { BotSnapshot, SshHost } from "../api/types";
import { badgeForBot, networkLabel, STATUS_BADGE_CLASS } from "../lib/constants";
import type { ToastEntry } from "./ToastShelf";
import { BotLogDialog } from "./BotLogDialog";
import { ConfirmDialog } from "./ConfirmDialog";

interface TerminatedBotsTableProps {
  /**
   * Already-filtered list of `done` / `failed` bots. BotsPage owns the
   * partition (so the same source-of-truth feeds both Running and
   * Terminated tables); this component just renders.
   */
  bots: BotSnapshot[];
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

/**
 * Read-only-ish sibling of {@link ./RunningBotsTable}. Renders
 * `done` / `failed` registry entries with their finish reason + a
 * "View logs" button + a per-row Remove (drop from registry). No live
 * actions (mute / camera / share / leave / force-kill) because the bot
 * is already gone.
 *
 * The bots prop is expected to be pre-sorted newest-first by the
 * parent (BotsPage). We re-sort defensively so this component is also
 * usable standalone (e.g. from tests).
 */
export function TerminatedBotsTable({ bots, onToast }: TerminatedBotsTableProps) {
  const [logDialogBot, setLogDialogBot] = useState<BotSnapshot | null>(null);
  const [confirmRemove, setConfirmRemove] = useState<BotSnapshot | null>(null);

  // Re-render every 30s so the "X minutes ago" column ticks forward
  // even when the parent's bots-poll didn't surface new data.
  const [, setTick] = useState(0);
  useEffect(() => {
    const handle = setInterval(() => setTick((t) => t + 1), 30_000);
    return () => clearInterval(handle);
  }, []);

  const qc = useQueryClient();
  const refreshBots = (): void => {
    void qc.invalidateQueries({ queryKey: ["bots"] });
  };

  // The drop-from-registry path reuses DELETE /api/bots/:id — the
  // server treats it as idempotent for already-terminated bots.
  const removeOne = useMutation({
    mutationFn: (botId: string) => api.kill(botId),
    onSuccess: () => {
      onToast({ title: "Removed from registry", variant: "success" });
      refreshBots();
    },
    onError: (err) =>
      onToast({
        title: "Remove failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  // Host registry lookup so the Host column tooltip can resolve
  // `ssh:<label>` → `<user>@<host>`. Shared cache with the other panels.
  const hostsQuery = useQuery({
    queryKey: ["ssh", "hosts"],
    queryFn: api.listHosts,
    retry: false,
    refetchInterval: 60_000,
  });
  const hostsByLabel = useMemo(() => {
    const map = new Map<string, SshHost>();
    for (const h of hostsQuery.data?.hosts ?? []) map.set(h.label, h);
    return map;
  }, [hostsQuery.data?.hosts]);

  const sortedBots = useMemo(() => {
    return [...bots].sort((a, b) => (b.finishedAt ?? 0) - (a.finishedAt ?? 0));
  }, [bots]);

  if (sortedBots.length === 0) {
    return (
      <div
        className="px-6 py-10 text-center text-sm text-neutral-500 dark:text-slate-400"
        data-testid="terminated-bots-empty"
      >
        No terminated bots yet. Bots that finish will appear here for the last hour.
      </div>
    );
  }

  return (
    <>
      <div className="overflow-x-auto" data-testid="terminated-bots-table">
        <table className="w-full text-sm">
          <thead className="bg-neutral-50 text-xs uppercase tracking-wide text-neutral-500 dark:bg-slate-900 dark:text-slate-400">
            <tr>
              <th className="px-4 py-2 text-left font-medium">Status</th>
              <th className="px-4 py-2 text-left font-medium">Bot</th>
              <th className="px-4 py-2 text-left font-medium">Participant</th>
              <th className="px-4 py-2 text-left font-medium">Meeting</th>
              <th className="px-4 py-2 text-left font-medium">Net</th>
              <th className="px-4 py-2 text-left font-medium">Host</th>
              <th className="px-4 py-2 text-left font-medium">Finished</th>
              <th className="px-4 py-2 text-right font-medium">Actions</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-neutral-100 dark:divide-slate-700">
            {sortedBots.map((b) => {
              const remote = b.host?.kind === "ssh";
              const remoteHost =
                remote && b.host?.kind === "ssh"
                  ? (hostsByLabel.get(b.host.hostLabel) ?? null)
                  : null;
              const { label, badgeKey } = badgeForBot(b);
              return (
                <tr
                  key={b.botId}
                  className="hover:bg-neutral-50 dark:hover:bg-slate-700/50"
                  data-testid={`terminated-bot-row-${b.botId}`}
                >
                  <td className="px-4 py-2">
                    <span
                      className={`inline-flex whitespace-nowrap rounded-full border px-2.5 py-0.5 text-xs font-medium ${
                        STATUS_BADGE_CLASS[badgeKey] ?? STATUS_BADGE_CLASS.done
                      }`}
                      title={b.finishReason ? `finishReason: ${b.finishReason}` : undefined}
                    >
                      {label}
                    </span>
                    {b.finishReason && (
                      <p
                        className="mt-1 max-w-xs truncate font-mono text-[11px] text-neutral-500 dark:text-slate-400"
                        title={b.finishReason}
                      >
                        {b.finishReason}
                      </p>
                    )}
                    {b.lastError && (
                      <p
                        className="mt-1 max-w-xs truncate text-xs text-red-600 dark:text-red-400"
                        title={b.lastError}
                      >
                        {b.lastError}
                      </p>
                    )}
                  </td>
                  <td
                    className="px-4 py-2 font-mono text-xs text-neutral-600 dark:text-slate-400"
                    title={b.botId}
                  >
                    {b.botId.slice(0, 8)}
                  </td>
                  <td className="px-4 py-2 text-neutral-800 dark:text-slate-200">
                    {b.participant}
                  </td>
                  <td className="px-4 py-2">
                    <a
                      href={b.meetingURL}
                      target="_blank"
                      rel="noreferrer"
                      className="inline-flex items-center gap-1 text-sky-600 hover:underline dark:text-sky-400"
                    >
                      <span className="max-w-xs truncate">{meetingLabel(b.meetingURL)}</span>
                      <ExternalLink className="h-3 w-3" />
                    </a>
                  </td>
                  <td className="px-4 py-2 text-xs text-neutral-600 dark:text-slate-400">
                    {b.network ? networkLabel(b.network) : "—"}
                  </td>
                  <td className="px-4 py-2">
                    {remote && b.host?.kind === "ssh" ? (
                      <span
                        className="inline-flex whitespace-nowrap rounded-full border border-violet-200 bg-violet-100 px-2.5 py-0.5 text-xs font-medium text-violet-800 dark:border-violet-800 dark:bg-violet-900/30 dark:text-violet-200"
                        title={
                          remoteHost
                            ? `ssh ${remoteHost.user}@${remoteHost.host}`
                            : `ssh host "${b.host.hostLabel}" (not in registry)`
                        }
                        data-testid={`terminated-bot-host-chip-${b.botId}`}
                      >
                        ssh:{b.host.hostLabel}
                      </span>
                    ) : (
                      <span
                        className="inline-flex whitespace-nowrap rounded-full border border-neutral-200 bg-neutral-100 px-2.5 py-0.5 text-xs font-medium text-neutral-700 dark:border-slate-600 dark:bg-slate-700 dark:text-slate-300"
                        title="Local Playwright bot"
                        data-testid={`terminated-bot-host-chip-${b.botId}`}
                      >
                        local
                      </span>
                    )}
                  </td>
                  <td
                    className="px-4 py-2 text-xs text-neutral-600 dark:text-slate-400"
                    data-testid={`terminated-bot-finished-${b.botId}`}
                  >
                    {formatFinishedAt(b.finishedAt)}
                  </td>
                  <td className="px-4 py-2">
                    <div className="flex justify-end gap-1">
                      <IconButton
                        title="View logs"
                        onClick={() => setLogDialogBot(b)}
                        testId={`terminated-bot-view-log-${b.botId}`}
                      >
                        <FileText className="h-4 w-4" />
                      </IconButton>
                      <IconButton
                        title="Remove from registry"
                        onClick={() => setConfirmRemove(b)}
                        destructive
                        testId={`terminated-bot-remove-${b.botId}`}
                      >
                        <Trash2 className="h-4 w-4" />
                      </IconButton>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <BotLogDialog bot={logDialogBot} onClose={() => setLogDialogBot(null)} />
      <ConfirmDialog
        open={confirmRemove !== null}
        title="Remove from registry?"
        body={
          confirmRemove
            ? `Drop bot ${confirmRemove.participant} (${confirmRemove.botId.slice(0, 8)}) from the registry. The logs will no longer be available afterwards.`
            : ""
        }
        confirmLabel="Remove"
        destructive
        onCancel={() => setConfirmRemove(null)}
        onConfirm={() => {
          if (confirmRemove) removeOne.mutate(confirmRemove.botId);
          setConfirmRemove(null);
        }}
      />
    </>
  );
}

interface IconButtonProps {
  title: string;
  onClick: () => void;
  destructive?: boolean;
  testId?: string;
  children: React.ReactNode;
}

function IconButton({ title, onClick, destructive, testId, children }: IconButtonProps) {
  const base =
    "inline-flex h-8 w-8 items-center justify-center rounded-md border text-sm transition-colors focus:outline-none focus:ring-1 focus:ring-sky-500";
  const cls = destructive
    ? "border-red-200 bg-white text-red-600 hover:bg-red-50 dark:border-red-800 dark:bg-slate-800 dark:text-red-400 dark:hover:bg-red-900/30"
    : "border-neutral-200 bg-white text-neutral-600 hover:bg-neutral-50 hover:text-neutral-900 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700 dark:hover:text-slate-100";
  return (
    <button
      type="button"
      title={title}
      aria-label={title}
      onClick={onClick}
      data-testid={testId}
      className={`${base} ${cls}`}
    >
      {children}
    </button>
  );
}

function meetingLabel(url: string): string {
  try {
    const u = new URL(url);
    const parts = u.pathname.split("/").filter(Boolean);
    const idx = parts.indexOf("meeting");
    const id = idx >= 0 && idx + 1 < parts.length ? parts[idx + 1] : parts[parts.length - 1];
    return `${u.host}/${id ?? ""}`;
  } catch {
    return url;
  }
}

/**
 * Render `finishedAt` as a coarse "X minutes ago" relative timestamp.
 * Exported for unit tests so the rounding rules are pinned down.
 */
export function formatFinishedAt(
  finishedAt: number | null | undefined,
  now: number = Date.now(),
): string {
  if (finishedAt === null || finishedAt === undefined) return "—";
  const deltaMs = Math.max(0, now - finishedAt);
  const seconds = Math.floor(deltaMs / 1_000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min${minutes === 1 ? "" : "s"} ago`;
  const hours = Math.floor(minutes / 60);
  const remMinutes = minutes % 60;
  if (hours < 24) {
    return remMinutes === 0
      ? `${hours} h${hours === 1 ? "" : "rs"} ago`
      : `${hours}h ${remMinutes}m ago`;
  }
  // Should never trigger in practice because retention is 1h, but keep
  // the path legible if the policy ever changes.
  const days = Math.floor(hours / 24);
  return `${days} day${days === 1 ? "" : "s"} ago`;
}
