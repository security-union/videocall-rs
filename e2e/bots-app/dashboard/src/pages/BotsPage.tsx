import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, ChevronUp } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import { BulkActionsToolbar, type OptimisticState } from "../components/BulkActionsToolbar";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { LaunchForm, type LaunchFormInitial } from "../components/LaunchForm";
import { MultiLaunchForm } from "../components/MultiLaunchForm";
import { RunningBotsTable } from "../components/RunningBotsTable";
import { RunProfiles } from "../components/RunProfiles";
import { TerminatedBotsTable } from "../components/TerminatedBotsTable";
import { ToastShelf, useToastShelf } from "../components/ToastShelf";

/**
 * Status keys that mean "still running" — used to partition the bots
 * list between the Running and Terminated sections. Anything NOT in
 * this set (i.e. `done` / `failed` / any future terminal status the
 * server might add) is treated as Terminated.
 */
const RUNNING_STATUSES = new Set(["priming", "launching", "joining", "in-meeting", "leaving"]);

/**
 * Soft cap on the number of terminated entries the orchestrator keeps
 * in memory. Mirrors `TERMINATED_REGISTRY_CAP` on the Node side; used
 * here purely for the header's `<N>/100` count badge. Drift between
 * the two surfaces as a confusing UX (badge says "112 / 100") but does
 * NOT break correctness — the server still enforces the cap.
 */
const TERMINATED_CAP = 100;

export function BotsPage() {
  const toast = useToastShelf();
  const botsQuery = useQuery({
    queryKey: ["bots"],
    queryFn: api.listBots,
    // Phase-4 ctl API is happy with frequent polls; the registry
    // sweep runs lazily on the request path, not a timer.
    refetchInterval: 2_500,
  });

  // Stable reference for the partition memos. Without this, the
  // logical-or fallback creates a fresh `[]` on every render and the
  // memos invalidate every refetch tick.
  const allBots = useMemo(() => botsQuery.data?.bots ?? [], [botsQuery.data?.bots]);
  const liveBots = useMemo(() => allBots.filter((b) => RUNNING_STATUSES.has(b.status)), [allBots]);
  const terminatedBots = useMemo(
    () =>
      allBots
        .filter((b) => !RUNNING_STATUSES.has(b.status))
        .sort((a, b) => (b.finishedAt ?? 0) - (a.finishedAt ?? 0)),
    [allBots],
  );
  const totalBots = liveBots.length + terminatedBots.length;
  const hasLive = liveBots.length > 0;

  const qc = useQueryClient();
  const clearTerminated = useMutation({
    mutationFn: () => api.clearTerminatedBots(),
    onSuccess: (data) => {
      toast.push({
        title:
          data.removedCount === 0
            ? "No terminated bots to clear"
            : `Cleared ${data.removedCount} terminated bot${data.removedCount === 1 ? "" : "s"}`,
        variant: data.removedCount === 0 ? "info" : "success",
      });
      void qc.invalidateQueries({ queryKey: ["bots"] });
    },
    onError: (err) =>
      toast.push({
        title: "Clear failed",
        description: err instanceof DashboardApiError ? err.message : (err as Error).message,
        variant: "error",
      }),
  });
  const [confirmClearAll, setConfirmClearAll] = useState(false);

  // Collapsible launch form. We collapse it automatically once at
  // least one bot is running so the running-list is what the operator
  // sees on a return visit; manual toggle wins thereafter.
  const [launchOpen, setLaunchOpen] = useState(true);
  const [userOverride, setUserOverride] = useState(false);
  const effectiveOpen = userOverride ? launchOpen : !hasLive;
  const handleToggle = () => {
    setUserOverride(true);
    setLaunchOpen(!effectiveOpen);
  };

  const [initialValues, setInitialValues] = useState<LaunchFormInitial | undefined>(undefined);

  // Multi-launch (first-N + random-N) section. Defaults to collapsed
  // when bots are already running so the operator's first action on a
  // return visit is the running-bots table, not yet another form. The
  // operator can flip it open from the section header at any time.
  const [multiOpen, setMultiOpen] = useState(false);

  // Optimistic per-bot mic/camera/share state. Owned here (rather
  // than inside `RunningBotsTable`) so the `BulkActionsToolbar` and
  // the per-row icons share a single source of truth — a "Mute all"
  // from the toolbar flips the per-row mic icons immediately.
  const [optimistic, setOptimistic] = useState<OptimisticState>({});

  return (
    <div className="flex flex-col gap-8">
      {/* ─── Launching group ───────────────────────────────────────────
          The two launch surfaces (multi-launch + single-launch) form
          a "compose what to spawn" mental block. Grouping them under a
          shared eyebrow label + a subtle background tint visually
          separates "actions you take" from "things currently
          happening" (the running group further down). The wrapper is
          purely structural — section-level data-testids / aria-labels
          on each form remain at their established locations so any
          existing E2E selectors stay valid. */}
      <div
        className="flex flex-col gap-6 rounded-xl bg-sky-50/40 p-4 dark:bg-slate-900/40"
        data-testid="launching-group"
      >
        <h2 className="px-1 text-xs font-semibold uppercase tracking-wider text-sky-700 dark:text-sky-300">
          Launching
        </h2>
        <section
          aria-label="Multi-launch"
          className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
          data-testid="multi-launch-section"
        >
          <button
            type="button"
            onClick={() => setMultiOpen((v) => !v)}
            className="flex w-full items-center justify-between px-6 py-4 text-left"
            aria-expanded={multiOpen}
            data-testid="multi-launch-toggle"
          >
            <div>
              <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
                Multi-launch
              </h2>
              <p className="text-sm text-neutral-500 dark:text-slate-400">
                Spawn N bots from the manifest in one click — first-N (deterministic) or random-N
                (seeded). Matches the CLI&apos;s{" "}
                <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                  bots-app run --users N
                </code>{" "}
                and{" "}
                <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                  bots-app gen --count N --seed S
                </code>
                .
              </p>
            </div>
            {multiOpen ? (
              <ChevronUp className="h-5 w-5 text-neutral-400 dark:text-slate-500" />
            ) : (
              <ChevronDown className="h-5 w-5 text-neutral-400 dark:text-slate-500" />
            )}
          </button>
          {multiOpen && (
            <div className="border-t border-neutral-200 px-6 py-5 dark:border-slate-700">
              <MultiLaunchForm
                onToast={(t) => toast.push(t)}
                onLaunched={(resp) => {
                  const launched = resp.botIds.length;
                  const namesList = resp.participants.slice(0, 5).join(", ");
                  const tail =
                    resp.participants.length > 5 ? `, +${resp.participants.length - 5} more` : "";
                  const seedLine =
                    resp.mode === "random" && resp.seed !== null ? ` (seed=${resp.seed})` : "";
                  toast.push({
                    title:
                      launched === resp.count
                        ? `Launched ${launched} bots`
                        : `Launched ${launched}/${resp.count} bots`,
                    description: `${namesList}${tail}${seedLine}`,
                    variant: resp.errors.length > 0 ? "info" : "success",
                  });
                  if (resp.errors.length > 0) {
                    for (const err of resp.errors) {
                      toast.push({
                        title: `Skipped ${err.participant}`,
                        description: err.message,
                        variant: "error",
                      });
                    }
                  }
                  botsQuery.refetch();
                }}
                onError={(message) =>
                  toast.push({
                    title: "Multi-launch failed",
                    description: message,
                    variant: "error",
                  })
                }
              />
            </div>
          )}
        </section>

        <section
          aria-label="Launch a Bot"
          className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
        >
          <button
            type="button"
            onClick={handleToggle}
            className="flex w-full items-center justify-between px-6 py-4 text-left"
            aria-expanded={effectiveOpen}
            data-testid="launch-form-toggle"
          >
            <div>
              <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
                Launch a Bot
              </h2>
              <p className="text-sm text-neutral-500 dark:text-slate-400">
                Configure a meeting attendee and where it should run.
              </p>
            </div>
            {effectiveOpen ? (
              <ChevronUp className="h-5 w-5 text-neutral-400 dark:text-slate-500" />
            ) : (
              <ChevronDown className="h-5 w-5 text-neutral-400 dark:text-slate-500" />
            )}
          </button>
          {effectiveOpen && (
            <div className="border-t border-neutral-200 px-6 py-5 dark:border-slate-700">
              <LaunchForm
                initialValues={initialValues}
                onToast={(t) => toast.push(t)}
                onLaunched={(botId) => {
                  toast.push({
                    title: "Bot launched",
                    description: `Bot ${botId.slice(0, 8)} is starting…`,
                    variant: "success",
                  });
                  botsQuery.refetch();
                }}
                onError={(message) =>
                  toast.push({ title: "Launch failed", description: message, variant: "error" })
                }
              />
            </div>
          )}
        </section>
      </div>

      {/* ─── Running group ─────────────────────────────────────────────
          "Things currently happening" — saved run profiles, live
          bots, and recently-terminated bots. Distinct background tint
          from the launching group above so the operator can visually
          jump straight to either half without having to read the
          section headers. */}
      <div
        className="flex flex-col gap-6 rounded-xl bg-emerald-50/30 p-4 dark:bg-slate-900/40"
        data-testid="running-group"
      >
        <h2 className="px-1 text-xs font-semibold uppercase tracking-wider text-emerald-700 dark:text-emerald-300">
          Running
        </h2>
        <RunProfiles
          hasBots={(botsQuery.data?.bots ?? []).length > 0}
          onToast={(t) => toast.push(t)}
        />

        <section
          aria-label="Running Bots"
          className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
        >
          <div className="flex items-center justify-between px-6 py-4">
            <div>
              <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
                Running Bots
              </h2>
              <p className="text-sm text-neutral-500 dark:text-slate-400">
                Live bots currently in the orchestrator&apos;s registry. Refreshes every 2.5s.
              </p>
            </div>
            <span
              className="rounded-full border border-neutral-200 bg-neutral-50 px-3 py-1 font-mono text-xs text-neutral-600 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-300"
              data-testid="running-bots-count-badge"
            >
              {liveBots.length} live / {totalBots} total
            </span>
          </div>
          <BulkActionsToolbar
            bots={liveBots}
            optimistic={optimistic}
            setOptimistic={setOptimistic}
            onToast={(t) => toast.push(t)}
          />
          <div className="border-t border-neutral-200 dark:border-slate-700">
            <RunningBotsTable
              isLoading={botsQuery.isLoading}
              error={botsQuery.error}
              bots={liveBots}
              optimistic={optimistic}
              setOptimistic={setOptimistic}
              onDuplicate={(snap) => {
                setInitialValues({
                  meetingURL: snap.meetingURL,
                  participant: snap.participant,
                  displayName: snap.participant,
                  ttl: snap.ttl,
                  network: snap.network ?? "none",
                  headless: false,
                  authBackend: "jwt",
                  storageStateFile: "",
                  runLocation: "local",
                  sshHostLabel: "",
                  costume: "default",
                  audio: "default",
                });
                setUserOverride(true);
                setLaunchOpen(true);
                toast.push({
                  title: "Form pre-filled",
                  description: `Tweak any field then click Launch Bot to spawn a copy of ${snap.participant}.`,
                  variant: "info",
                });
              }}
              onToast={(t) => toast.push(t)}
            />
          </div>
        </section>

        <section
          aria-label="Terminated Bots"
          className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
          data-testid="terminated-bots-section"
        >
          <div className="flex items-center justify-between px-6 py-4">
            <div>
              <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
                Terminated Bots
              </h2>
              <p className="text-sm text-neutral-500 dark:text-slate-400">
                Bots that have ended in the last hour. Logs remain available here for post-mortem
                inspection.
              </p>
            </div>
            <div className="flex items-center gap-2">
              <span
                className="rounded-full border border-neutral-200 bg-neutral-50 px-3 py-1 font-mono text-xs text-neutral-600 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-300"
                data-testid="terminated-bots-count-badge"
              >
                {terminatedBots.length} / {TERMINATED_CAP}
              </span>
              <button
                type="button"
                onClick={() => setConfirmClearAll(true)}
                disabled={terminatedBots.length === 0 || clearTerminated.isPending}
                data-testid="terminated-bots-clear-all"
                className="rounded-md border border-neutral-300 bg-white px-3 py-1 text-xs font-medium text-neutral-700 hover:bg-neutral-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
              >
                Clear all
              </button>
            </div>
          </div>
          <div className="border-t border-neutral-200 dark:border-slate-700">
            <TerminatedBotsTable bots={terminatedBots} onToast={(t) => toast.push(t)} />
          </div>
        </section>
      </div>

      <ConfirmDialog
        open={confirmClearAll}
        title="Clear all terminated bots?"
        body={`Drop ${terminatedBots.length} terminated bot${
          terminatedBots.length === 1 ? "" : "s"
        } from the registry. Their logs will no longer be available afterwards.`}
        confirmLabel="Clear all"
        destructive
        onCancel={() => setConfirmClearAll(false)}
        onConfirm={() => {
          setConfirmClearAll(false);
          clearTerminated.mutate();
        }}
      />

      <ToastShelf entries={toast.entries} onDismiss={toast.dismiss} />
    </div>
  );
}
