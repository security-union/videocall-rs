import { useEffect, useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Copy, ExternalLink, LogOut, Mic, MicOff, Monitor, Timer, Trash2, Video, VideoOff } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { BotSnapshot } from "../api/types";
import { badgeForBot, networkLabel, STATUS_BADGE_CLASS } from "../lib/constants";
import { formatRemaining } from "../lib/ttl";
import type { ToastEntry } from "./ToastShelf";
import { ExtendTtlDialog } from "./ExtendTtlDialog";
import { ConfirmDialog } from "./ConfirmDialog";

interface RunningBotsTableProps {
  isLoading: boolean;
  error: unknown;
  bots: BotSnapshot[];
  onDuplicate: (snap: BotSnapshot) => void;
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

export function RunningBotsTable({
  isLoading,
  error,
  bots,
  onDuplicate,
  onToast,
}: RunningBotsTableProps) {
  // Local ticker so the TTL countdown updates between server polls.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const handle = setInterval(() => setNow(Date.now()), 1_000);
    return () => clearInterval(handle);
  }, []);

  // Per-bot "what action did we just dispatch" tracking. The
  // mute/video/share endpoints don't echo state back beyond the value
  // we sent in, so the optimistic UI state is kept locally and
  // reconciled on the next poll via `bots[].status`.
  const [optimistic, setOptimistic] = useState<
    Record<string, { mic?: boolean; camera?: boolean; share?: boolean }>
  >({});

  const qc = useQueryClient();
  const refreshBots = () => qc.invalidateQueries({ queryKey: ["bots"] });

  const leave = useMutation({
    mutationFn: (botId: string) => api.leave(botId),
    onSuccess: () => {
      onToast({ title: "Leave requested", variant: "success" });
      refreshBots();
    },
    onError: (err) =>
      onToast({
        title: "Leave failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  const kill = useMutation({
    mutationFn: (botId: string) => api.kill(botId),
    onSuccess: () => {
      onToast({ title: "Kill requested", variant: "success" });
      refreshBots();
    },
    onError: (err) =>
      onToast({
        title: "Kill failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  const setMic = useMutation({
    mutationFn: ({ botId, mic }: { botId: string; mic: boolean }) => api.setMic(botId, mic),
    onMutate: ({ botId, mic }) => {
      setOptimistic((p) => ({ ...p, [botId]: { ...p[botId], mic } }));
    },
    onError: (err, vars) => {
      setOptimistic((p) => ({ ...p, [vars.botId]: { ...p[vars.botId], mic: undefined } }));
      onToast({
        title: "Mic toggle failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      });
    },
  });

  const setCamera = useMutation({
    mutationFn: ({ botId, camera }: { botId: string; camera: boolean }) =>
      api.setCamera(botId, camera),
    onMutate: ({ botId, camera }) => {
      setOptimistic((p) => ({ ...p, [botId]: { ...p[botId], camera } }));
    },
    onError: (err, vars) => {
      setOptimistic((p) => ({ ...p, [vars.botId]: { ...p[vars.botId], camera: undefined } }));
      onToast({
        title: "Camera toggle failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      });
    },
  });

  const setShare = useMutation({
    mutationFn: ({ botId, share }: { botId: string; share: boolean }) => api.setShare(botId, share),
    onMutate: ({ botId, share }) => {
      setOptimistic((p) => ({ ...p, [botId]: { ...p[botId], share } }));
    },
    onError: (err, vars) => {
      setOptimistic((p) => ({ ...p, [vars.botId]: { ...p[vars.botId], share: undefined } }));
      onToast({
        title: "Share toggle failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      });
    },
  });

  const setTtl = useMutation({
    mutationFn: ({ botId, body }: { botId: string; body: { ttl?: string; extendBy?: string } }) =>
      api.setTtl(botId, body),
    onSuccess: (data) => {
      onToast({ title: "TTL updated", description: `→ ${data.ttl}`, variant: "success" });
      refreshBots();
    },
    onError: (err) =>
      onToast({
        title: "TTL update failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });

  const [ttlDialogBot, setTtlDialogBot] = useState<BotSnapshot | null>(null);
  const [confirmLeave, setConfirmLeave] = useState<BotSnapshot | null>(null);
  const [confirmKill, setConfirmKill] = useState<BotSnapshot | null>(null);

  const sortedBots = useMemo(() => {
    return [...bots].sort((a, b) => a.startedAt - b.startedAt);
  }, [bots]);

  if (error) {
    return (
      <div className="px-6 py-6 text-sm text-red-700 dark:text-red-300">
        Could not fetch the bot list:{" "}
        <code className="font-mono text-xs">
          {error instanceof Error ? error.message : String(error)}
        </code>
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="px-6 py-6 text-sm text-neutral-500 dark:text-slate-400">Loading bots…</div>
    );
  }

  if (sortedBots.length === 0) {
    return (
      <div className="px-6 py-10 text-center text-sm text-neutral-500 dark:text-slate-400">
        No bots yet. Use the Launch a Bot form above to start one.
      </div>
    );
  }

  return (
    <>
      <div className="overflow-x-auto" data-testid="running-bots-table">
        <table className="w-full text-sm">
          <thead className="bg-neutral-50 text-xs uppercase tracking-wide text-neutral-500 dark:bg-slate-900 dark:text-slate-400">
            <tr>
              <th className="px-4 py-2 text-left font-medium">Status</th>
              <th className="px-4 py-2 text-left font-medium">Bot</th>
              <th className="px-4 py-2 text-left font-medium">Participant</th>
              <th className="px-4 py-2 text-left font-medium">Meeting</th>
              <th className="px-4 py-2 text-left font-medium">TTL</th>
              <th className="px-4 py-2 text-left font-medium">Net</th>
              <th className="px-4 py-2 text-right font-medium">Actions</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-neutral-100 dark:divide-slate-700">
            {sortedBots.map((b) => {
              const inMeeting = b.status === "in-meeting";
              const terminal = b.status === "done" || b.status === "failed";
              const opt = optimistic[b.botId] ?? {};
              // The snapshot endpoint returns `ttlRemainingMs` at the
              // moment of the response; smooth between polls by
              // subtracting the local elapsed time since this `now`
              // tick was last refreshed. The poll runs every 2.5s,
              // which keeps the drift well under the human eye.
              void now;
              return (
                <tr key={b.botId} className="hover:bg-neutral-50 dark:hover:bg-slate-700/50">
                  <td className="px-4 py-2">
                    {(() => {
                      const { label, badgeKey } = badgeForBot(b);
                      return (
                        <span
                          className={`inline-flex whitespace-nowrap rounded-full border px-2.5 py-0.5 text-xs font-medium ${
                            STATUS_BADGE_CLASS[badgeKey] ?? STATUS_BADGE_CLASS.done
                          }`}
                          title={b.finishReason ? `finishReason: ${b.finishReason}` : undefined}
                        >
                          {label}
                        </span>
                      );
                    })()}
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
                  <td className="px-4 py-2 text-neutral-800 dark:text-slate-200">{b.participant}</td>
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
                  <td className="px-4 py-2 font-mono text-xs text-neutral-700 dark:text-slate-300">
                    {formatRemaining(b.ttlRemainingMs)}
                  </td>
                  <td className="px-4 py-2 text-xs text-neutral-600 dark:text-slate-400">
                    {b.network ? networkLabel(b.network) : "—"}
                  </td>
                  <td className="px-4 py-2">
                    <div className="flex justify-end gap-1">
                      <IconButton
                        title="Extend / set TTL"
                        onClick={() => setTtlDialogBot(b)}
                        disabled={terminal}
                      >
                        <Timer className="h-4 w-4" />
                      </IconButton>
                      <IconButton
                        title={opt.mic ? "Unmute mic" : "Mute mic"}
                        onClick={() =>
                          setMic.mutate({ botId: b.botId, mic: !(opt.mic ?? false) })
                        }
                        disabled={!inMeeting}
                        active={opt.mic === true}
                      >
                        {opt.mic ? <MicOff className="h-4 w-4" /> : <Mic className="h-4 w-4" />}
                      </IconButton>
                      <IconButton
                        title={opt.camera ? "Camera on" : "Camera off"}
                        onClick={() =>
                          setCamera.mutate({ botId: b.botId, camera: !(opt.camera ?? false) })
                        }
                        disabled={!inMeeting}
                        active={opt.camera === true}
                      >
                        {opt.camera ? (
                          <VideoOff className="h-4 w-4" />
                        ) : (
                          <Video className="h-4 w-4" />
                        )}
                      </IconButton>
                      <IconButton
                        title={opt.share ? "Stop sharing" : "Share screen"}
                        onClick={() =>
                          setShare.mutate({ botId: b.botId, share: !(opt.share ?? false) })
                        }
                        disabled={!inMeeting}
                        active={opt.share === true}
                      >
                        <Monitor className="h-4 w-4" />
                      </IconButton>
                      <IconButton
                        title="Duplicate (pre-fill launch form)"
                        onClick={() => onDuplicate(b)}
                      >
                        <Copy className="h-4 w-4" />
                      </IconButton>
                      <IconButton
                        title="Leave meeting"
                        onClick={() => setConfirmLeave(b)}
                        disabled={terminal}
                      >
                        <LogOut className="h-4 w-4" />
                      </IconButton>
                      <IconButton
                        title="Remove (force kill)"
                        onClick={() => setConfirmKill(b)}
                        disabled={terminal}
                        destructive
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

      <ExtendTtlDialog
        bot={ttlDialogBot}
        onClose={() => setTtlDialogBot(null)}
        onSubmit={(body) => {
          if (!ttlDialogBot) return;
          setTtl.mutate({ botId: ttlDialogBot.botId, body });
          setTtlDialogBot(null);
        }}
      />
      <ConfirmDialog
        open={confirmLeave !== null}
        title="Leave meeting?"
        body={
          confirmLeave
            ? `Bot ${confirmLeave.participant} (${confirmLeave.botId.slice(0, 8)}) will gracefully leave the meeting and shut down.`
            : ""
        }
        confirmLabel="Leave"
        onCancel={() => setConfirmLeave(null)}
        onConfirm={() => {
          if (confirmLeave) leave.mutate(confirmLeave.botId);
          setConfirmLeave(null);
        }}
      />
      <ConfirmDialog
        open={confirmKill !== null}
        title="Force-kill bot?"
        body={
          confirmKill
            ? `Are you sure? This will force-kill bot ${confirmKill.participant} (${confirmKill.botId.slice(0, 8)}) — the bot will NOT leave the meeting cleanly.`
            : ""
        }
        confirmLabel="Force kill"
        destructive
        onCancel={() => setConfirmKill(null)}
        onConfirm={() => {
          if (confirmKill) kill.mutate(confirmKill.botId);
          setConfirmKill(null);
        }}
      />
    </>
  );
}

interface IconButtonProps {
  title: string;
  onClick: () => void;
  disabled?: boolean;
  active?: boolean;
  destructive?: boolean;
  children: React.ReactNode;
}

function IconButton({ title, onClick, disabled, active, destructive, children }: IconButtonProps) {
  const base =
    "inline-flex h-8 w-8 items-center justify-center rounded-md border text-sm transition-colors focus:outline-none focus:ring-1 focus:ring-sky-500";
  let cls: string;
  if (disabled) {
    cls =
      "border-neutral-200 bg-neutral-50 text-neutral-300 cursor-not-allowed dark:border-slate-700 dark:bg-slate-800 dark:text-slate-600";
  } else if (destructive) {
    cls =
      "border-red-200 bg-white text-red-600 hover:bg-red-50 dark:border-red-800 dark:bg-slate-800 dark:text-red-400 dark:hover:bg-red-900/30";
  } else if (active) {
    cls =
      "border-sky-300 bg-sky-50 text-sky-700 dark:border-sky-700 dark:bg-sky-900/40 dark:text-sky-200";
  } else {
    cls =
      "border-neutral-200 bg-white text-neutral-600 hover:bg-neutral-50 hover:text-neutral-900 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-300 dark:hover:bg-slate-700 dark:hover:text-slate-100";
  }
  return (
    <button
      type="button"
      title={title}
      aria-label={title}
      onClick={onClick}
      disabled={disabled}
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
