import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronUp } from "lucide-react";

import { api } from "../api/client";
import { LaunchForm, type LaunchFormInitial } from "../components/LaunchForm";
import { MultiLaunchForm } from "../components/MultiLaunchForm";
import { RunningBotsTable } from "../components/RunningBotsTable";
import { RunProfiles } from "../components/RunProfiles";
import { ToastShelf, useToastShelf } from "../components/ToastShelf";

export function BotsPage() {
  const toast = useToastShelf();
  const botsQuery = useQuery({
    queryKey: ["bots"],
    queryFn: api.listBots,
    // Phase-4 ctl API is happy with frequent polls; the registry
    // sweep runs lazily on the request path, not a timer.
    refetchInterval: 2_500,
  });

  const liveBots = (botsQuery.data?.bots ?? []).filter(
    (b) => b.status !== "done" && b.status !== "failed",
  );
  const hasLive = liveBots.length > 0;

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

  return (
    <div className="flex flex-col gap-6">
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
              Spawn N bots from the manifest in one click — first-N (deterministic) or
              random-N (seeded). Matches the CLI&apos;s{" "}
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
              onLaunched={(resp) => {
                const launched = resp.botIds.length;
                const namesList = resp.participants.slice(0, 5).join(", ");
                const tail =
                  resp.participants.length > 5
                    ? `, +${resp.participants.length - 5} more`
                    : "";
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
              Live + recently-finished bots in the orchestrator&apos;s registry. Refreshes every
              2.5s.
            </p>
          </div>
          <span className="rounded-full border border-neutral-200 bg-neutral-50 px-3 py-1 font-mono text-xs text-neutral-600 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-300">
            {liveBots.length} live / {botsQuery.data?.bots.length ?? 0} total
          </span>
        </div>
        <div className="border-t border-neutral-200 dark:border-slate-700">
          <RunningBotsTable
            isLoading={botsQuery.isLoading}
            error={botsQuery.error}
            bots={botsQuery.data?.bots ?? []}
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

      <ToastShelf entries={toast.entries} onDismiss={toast.dismiss} />
    </div>
  );
}
