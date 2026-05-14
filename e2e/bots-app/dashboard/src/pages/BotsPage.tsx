import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronUp } from "lucide-react";

import { api } from "../api/client";
import { LaunchForm, type LaunchFormInitial } from "../components/LaunchForm";
import { RunningBotsTable } from "../components/RunningBotsTable";
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

  return (
    <div className="flex flex-col gap-6">
      <section
        aria-label="Launch a Bot"
        className="rounded-lg border border-neutral-200 bg-white shadow-sm"
      >
        <button
          type="button"
          onClick={handleToggle}
          className="flex w-full items-center justify-between px-6 py-4 text-left"
          aria-expanded={effectiveOpen}
          data-testid="launch-form-toggle"
        >
          <div>
            <h2 className="text-lg font-semibold tracking-tight text-neutral-900">Launch a Bot</h2>
            <p className="text-sm text-neutral-500">
              Configure a meeting attendee and where it should run.
            </p>
          </div>
          {effectiveOpen ? (
            <ChevronUp className="h-5 w-5 text-neutral-400" />
          ) : (
            <ChevronDown className="h-5 w-5 text-neutral-400" />
          )}
        </button>
        {effectiveOpen && (
          <div className="border-t border-neutral-200 px-6 py-5">
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

      <section
        aria-label="Running Bots"
        className="rounded-lg border border-neutral-200 bg-white shadow-sm"
      >
        <div className="flex items-center justify-between px-6 py-4">
          <div>
            <h2 className="text-lg font-semibold tracking-tight text-neutral-900">Running Bots</h2>
            <p className="text-sm text-neutral-500">
              Live + recently-finished bots in the orchestrator&apos;s registry. Refreshes every
              2.5s.
            </p>
          </div>
          <span className="rounded-full border border-neutral-200 bg-neutral-50 px-3 py-1 font-mono text-xs text-neutral-600">
            {liveBots.length} live / {botsQuery.data?.bots.length ?? 0} total
          </span>
        </div>
        <div className="border-t border-neutral-200">
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
