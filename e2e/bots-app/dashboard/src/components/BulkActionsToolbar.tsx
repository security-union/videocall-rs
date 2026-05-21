import { useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { LogOut, Mic, MicOff, Trash2, Video, VideoOff } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { BotSnapshot } from "../api/types";
import { ConfirmDialog } from "./ConfirmDialog";
import type { ToastEntry } from "./ToastShelf";

/**
 * Per-bot optimistic mic/camera state, owned by `BotsPage` and shared
 * between the per-row `RunningBotsTable` action buttons and this
 * toolbar so a bulk "Mute all" leaves the per-row UI in a consistent
 * state. Mirrors the `optimistic` shape in `RunningBotsTable`.
 */
export type OptimisticState = Record<string, { mic?: boolean; camera?: boolean; share?: boolean }>;

interface BulkActionsToolbarProps {
  bots: BotSnapshot[];
  optimistic: OptimisticState;
  setOptimistic: React.Dispatch<React.SetStateAction<OptimisticState>>;
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

/**
 * True when the bot snapshot is an SSH-hosted bot. Bulk actions skip
 * remote bots, matching the per-row convention in `RunningBotsTable`
 * where the mic / camera / share buttons are disabled with the
 * `REMOTE_DISABLED_TITLE` tooltip (remote-bot v1 doesn't yet wire the
 * runtime-control endpoints through the SSH transport).
 */
function isRemoteBot(snap: BotSnapshot): boolean {
  return snap.host?.kind === "ssh";
}

/**
 * Format the count suffix on the bulk-action button labels — keeps
 * the disabled-state and the live count visible at a glance without
 * the operator having to scan the per-row table.
 */
function describeFanout(succeeded: number, failed: number, skipped: number, verb: string): string {
  const parts: string[] = [];
  if (succeeded > 0) parts.push(`${succeeded} ${verb}`);
  if (failed > 0) parts.push(`${failed} failed`);
  if (skipped > 0) parts.push(`${skipped} skipped (remote)`);
  return parts.join(", ");
}

/**
 * Bulk-action toolbar that fans out the four most common per-row
 * actions over every running bot in one click. Hidden when no
 * controllable (local + in-meeting) bots are present.
 *
 * Dispatch strategy: client-side fan-out via `Promise.allSettled` so a
 * single transient ctl-API failure (e.g. a bot that transitioned to
 * `terminated` between the snapshot poll and the click) doesn't abort
 * the rest of the batch. Results are summarised in a single toast —
 * surfacing N toasts per N bots would shred the toast shelf and bury
 * the actually-novel information ("which bots failed").
 *
 * Remote (SSH) bots are skipped at the toolbar level rather than
 * sent-and-rejected, matching the per-row buttons in
 * `RunningBotsTable` (which are `disabled` for remote bots) and
 * sparing the ctl-API a wave of guaranteed-failure requests.
 */
export function BulkActionsToolbar({
  bots,
  optimistic,
  setOptimistic,
  onToast,
}: BulkActionsToolbarProps) {
  const qc = useQueryClient();
  const refreshBots = () => qc.invalidateQueries({ queryKey: ["bots"] });

  const [pendingAction, setPendingAction] = useState<
    null | "mute" | "camera" | "leave" | "terminate"
  >(null);
  const [confirmLeave, setConfirmLeave] = useState(false);
  const [confirmTerminate, setConfirmTerminate] = useState(false);

  // Partition the live bots into "controllable" (local, anything we
  // can actually act on) vs "skipped" (remote). The aggregate label
  // decisions and the fan-out target list both use `controllable`.
  const { controllable, remoteSkipped } = useMemo(() => {
    const ctrl: BotSnapshot[] = [];
    let skipped = 0;
    for (const b of bots) {
      if (isRemoteBot(b)) skipped += 1;
      else ctrl.push(b);
    }
    return { controllable: ctrl, remoteSkipped: skipped };
  }, [bots]);

  // Aggregate state for the toggle labels:
  // - mic: per-row default is "muted" (opt.mic falsy = muted, true =
  //   unmuted). If ANY controllable bot is currently unmuted in the
  //   optimistic state, the bulk button reads "Mute all".
  // - camera: same shape — per-row default is "camera off" (opt.camera
  //   falsy = off, true = on).
  const anyUnmuted = useMemo(
    () => controllable.some((b) => optimistic[b.botId]?.mic === true),
    [controllable, optimistic],
  );
  const anyCameraOn = useMemo(
    () => controllable.some((b) => optimistic[b.botId]?.camera === true),
    [controllable, optimistic],
  );

  if (controllable.length === 0) {
    // Hide the toolbar entirely rather than rendering four disabled
    // buttons — keeps the Running Bots section uncluttered when there
    // is nothing to act on. Operator still sees the count badge in
    // the section header above.
    return null;
  }

  /**
   * Run an async action against every controllable bot, collect the
   * per-bot outcomes via `Promise.allSettled`, and surface a single
   * summary toast. `update` is called after success on each bot —
   * used to keep the per-row optimistic state in sync.
   */
  const runFanout = async (
    actionId: "mute" | "camera" | "leave" | "terminate",
    verbPast: string,
    perBot: (b: BotSnapshot) => Promise<void> | null,
  ): Promise<void> => {
    setPendingAction(actionId);
    try {
      // Build the dispatch list — `perBot` may return null to skip a
      // bot (e.g. "Mute all" skips already-muted bots).
      const targets: Array<{ bot: BotSnapshot; p: Promise<void> }> = [];
      let preSkipped = 0;
      for (const bot of controllable) {
        const p = perBot(bot);
        if (p === null) {
          preSkipped += 1;
          continue;
        }
        targets.push({ bot, p });
      }

      const results = await Promise.allSettled(targets.map((t) => t.p));
      let succeeded = 0;
      let failed = 0;
      const errors: string[] = [];
      results.forEach((r, i) => {
        if (r.status === "fulfilled") {
          succeeded += 1;
        } else {
          failed += 1;
          const err = r.reason;
          const msg =
            err instanceof DashboardApiError
              ? err.message
              : err instanceof Error
                ? err.message
                : String(err);
          // Surface the first 3 errors verbatim; the rest are
          // summarized by count in the toast title.
          if (errors.length < 3) errors.push(`${targets[i].bot.participant}: ${msg}`);
        }
      });

      const summary = describeFanout(succeeded, failed, remoteSkipped + preSkipped, verbPast);
      onToast({
        title: failed > 0 ? `Bulk action partial: ${summary}` : `Bulk action: ${summary}`,
        description: errors.length > 0 ? errors.join("\n") : undefined,
        variant: failed > 0 ? (succeeded > 0 ? "info" : "error") : "success",
      });
      refreshBots();
    } finally {
      setPendingAction(null);
    }
  };

  const doMuteToggle = async () => {
    // Direction is derived ONCE at click time, not per-bot — keeps
    // the semantics symmetric with the label the operator just saw.
    const muteAll = anyUnmuted; // true → set mic=false; false → set mic=true (unmute)
    const target = muteAll ? false : true;
    const verb = muteAll ? "muted" : "unmuted";
    await runFanout("mute", verb, (b) => {
      const cur = optimistic[b.botId]?.mic ?? false; // default per-row: muted
      if (cur === target) return null; // already in the desired state
      // Optimistic update before the request — matches the per-row
      // mutation's `onMutate` so the icon flips immediately.
      setOptimistic((p) => ({ ...p, [b.botId]: { ...p[b.botId], mic: target } }));
      return api.setMic(b.botId, target).then(
        () => undefined,
        (err) => {
          // Revert the optimistic flag on failure so the per-row UI
          // doesn't claim the bot is in a state it isn't.
          setOptimistic((p) => ({ ...p, [b.botId]: { ...p[b.botId], mic: cur } }));
          throw err;
        },
      );
    });
  };

  const doCameraToggle = async () => {
    const turnOn = !anyCameraOn; // if all off, click turns them on; otherwise click turns them off
    const target = turnOn;
    const verb = turnOn ? "video on" : "video off";
    await runFanout("camera", verb, (b) => {
      const cur = optimistic[b.botId]?.camera ?? false;
      if (cur === target) return null;
      setOptimistic((p) => ({ ...p, [b.botId]: { ...p[b.botId], camera: target } }));
      return api.setCamera(b.botId, target).then(
        () => undefined,
        (err) => {
          setOptimistic((p) => ({ ...p, [b.botId]: { ...p[b.botId], camera: cur } }));
          throw err;
        },
      );
    });
  };

  const doLeaveAll = async () => {
    await runFanout("leave", "leaving", (b) =>
      api.leave(b.botId).then(
        () => undefined,
        (err) => {
          throw err;
        },
      ),
    );
  };

  const doTerminateAll = async () => {
    await runFanout("terminate", "terminated", (b) =>
      api.kill(b.botId).then(
        () => undefined,
        (err) => {
          throw err;
        },
      ),
    );
  };

  const baseBtn =
    "inline-flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-xs font-medium transition-colors focus:outline-none focus:ring-1 focus:ring-sky-500 disabled:cursor-not-allowed disabled:opacity-50";
  const neutralBtn =
    "border-neutral-300 bg-white text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700";
  const destructiveBtn =
    "border-red-300 bg-white text-red-700 hover:bg-red-50 dark:border-red-800 dark:bg-slate-800 dark:text-red-300 dark:hover:bg-red-900/30";

  const remoteHint =
    remoteSkipped > 0
      ? ` (${remoteSkipped} remote bot${remoteSkipped === 1 ? "" : "s"} skipped)`
      : "";

  return (
    <div
      className="flex flex-wrap items-center gap-2 border-t border-neutral-200 bg-neutral-50 px-6 py-3 dark:border-slate-700 dark:bg-slate-900/40"
      data-testid="bulk-actions-toolbar"
      aria-label="Bulk actions for running bots"
    >
      <span className="mr-1 text-xs font-medium text-neutral-500 dark:text-slate-400">
        Bulk actions ({controllable.length} bot{controllable.length === 1 ? "" : "s"}
        {remoteHint}):
      </span>

      <button
        type="button"
        onClick={doMuteToggle}
        disabled={pendingAction !== null}
        data-testid="bulk-mute-toggle"
        aria-label={anyUnmuted ? "Mute all running bots" : "Unmute all running bots"}
        className={`${baseBtn} ${neutralBtn}`}
      >
        {anyUnmuted ? <MicOff className="h-3.5 w-3.5" /> : <Mic className="h-3.5 w-3.5" />}
        {anyUnmuted ? "Mute all" : "Unmute all"}
      </button>

      <button
        type="button"
        onClick={doCameraToggle}
        disabled={pendingAction !== null}
        data-testid="bulk-camera-toggle"
        aria-label={
          anyCameraOn
            ? "Turn camera off for all running bots"
            : "Turn camera on for all running bots"
        }
        className={`${baseBtn} ${neutralBtn}`}
      >
        {anyCameraOn ? <VideoOff className="h-3.5 w-3.5" /> : <Video className="h-3.5 w-3.5" />}
        {anyCameraOn ? "Camera off all" : "Camera on all"}
      </button>

      <button
        type="button"
        onClick={() => setConfirmLeave(true)}
        disabled={pendingAction !== null}
        data-testid="bulk-leave-all"
        aria-label="Leave meeting for all running bots"
        className={`${baseBtn} ${neutralBtn}`}
      >
        <LogOut className="h-3.5 w-3.5" />
        Leave all
      </button>

      <button
        type="button"
        onClick={() => setConfirmTerminate(true)}
        disabled={pendingAction !== null}
        data-testid="bulk-terminate-all"
        aria-label="Terminate (force-kill) all running bots"
        className={`${baseBtn} ${destructiveBtn}`}
      >
        <Trash2 className="h-3.5 w-3.5" />
        Terminate all
      </button>

      <ConfirmDialog
        open={confirmLeave}
        title={`Leave meeting for ${controllable.length} bot${
          controllable.length === 1 ? "" : "s"
        }?`}
        body={`All ${controllable.length} controllable running bot${
          controllable.length === 1 ? "" : "s"
        } will gracefully leave the meeting and shut down.${remoteHint}`}
        confirmLabel="Leave all"
        onCancel={() => setConfirmLeave(false)}
        onConfirm={() => {
          setConfirmLeave(false);
          void doLeaveAll();
        }}
      />

      <ConfirmDialog
        open={confirmTerminate}
        title={`Force-kill ${controllable.length} bot${controllable.length === 1 ? "" : "s"}?`}
        body={`This will force-kill all ${controllable.length} controllable running bot${
          controllable.length === 1 ? "" : "s"
        }. They will NOT leave the meeting cleanly.${remoteHint} Proceed?`}
        confirmLabel="Terminate all"
        destructive
        onCancel={() => setConfirmTerminate(false)}
        onConfirm={() => {
          setConfirmTerminate(false);
          void doTerminateAll();
        }}
      />
    </div>
  );
}
