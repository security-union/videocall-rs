import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { ChevronDown, ChevronUp, Loader2, Play, Wrench, X } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { PrepAssetsJobStatus } from "../api/types";
import type { ToastEntry } from "./ToastShelf";
import { HelpPopover } from "./ui/HelpPopover";

interface PrepAssetsPanelProps {
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

/**
 * "Prep Assets" card for the Tools page. Mirrors the CLI's
 * `bots-app prep-assets` — kicks off an in-process job on the
 * orchestrator that regenerates per-participant stitched WAVs and
 * costume y4m files. Output streams into a modal via SSE so the
 * operator sees ffmpeg progress live.
 *
 * Cancel semantics: clicking Cancel only closes the modal. The
 * underlying job runs to completion in the background — ffmpeg is
 * expensive to restart, and the dashboard's polling/streaming layers
 * make a re-attach trivial.
 */
export function PrepAssetsPanel({ onToast }: PrepAssetsPanelProps) {
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [participants, setParticipants] = useState("");
  const [manifestPath, setManifestPath] = useState("");
  const [costumeSource, setCostumeSource] = useState("");
  const [outputDir, setOutputDir] = useState("");

  const [activeJobId, setActiveJobId] = useState<string | null>(null);

  const startMutation = useMutation({
    mutationFn: (body: { participants?: string[]; manifestPath?: string; costumeSource?: string; outputDir?: string }) =>
      api.prepAssetsStart(body),
    onSuccess: (data) => {
      setActiveJobId(data.jobId);
      onToast({
        title: "prep-assets started",
        description: `Job ${data.jobId.slice(0, 8)} kicked off — streaming logs…`,
        variant: "info",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      if (err instanceof DashboardApiError && err.status === 409) {
        // Server already had a job running — try to re-attach.
        const body = err.body as { error?: string } | undefined;
        const match = body?.error?.match(/prep-assets job ([0-9a-f-]+)/);
        if (match) {
          setActiveJobId(match[1]);
          onToast({
            title: "prep-assets already running",
            description: `Re-attaching to job ${match[1].slice(0, 8)}.`,
            variant: "info",
          });
          return;
        }
      }
      onToast({ title: "prep-assets failed to start", description: msg, variant: "error" });
    },
  });

  const handleStart = () => {
    const body: { participants?: string[]; manifestPath?: string; costumeSource?: string; outputDir?: string } = {};
    if (participants.trim() !== "") {
      body.participants = participants
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
    }
    if (manifestPath.trim() !== "") body.manifestPath = manifestPath.trim();
    if (costumeSource.trim() !== "") body.costumeSource = costumeSource.trim();
    if (outputDir.trim() !== "") body.outputDir = outputDir.trim();
    startMutation.mutate(body);
  };

  return (
    <section
      className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="prep-assets-section"
    >
      <div className="flex items-center gap-2 px-6 py-4">
        <Wrench className="h-5 w-5 text-sky-500" aria-hidden="true" />
        <div className="flex-1">
          <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
            Prep Assets
          </h2>
          <p className="text-sm text-neutral-500 dark:text-slate-400">
            Regenerate per-participant stitched WAV + costume y4m files. Mirrors{" "}
            <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
              bots-app prep-assets
            </code>
            . Heavy: spawns ffmpeg per costume — may take a few minutes.
          </p>
        </div>
        <HelpPopover fieldLabel="Prep Assets" testId="help-prep-assets">
          <p>Generates the fake-device files Chrome consumes via --use-file-for-fake-*-capture.</p>
          <p className="mt-1">
            Cached outputs are reused when the inputs haven&apos;t changed, so re-runs are cheap.
          </p>
        </HelpPopover>
      </div>
      <div className="border-t border-neutral-200 px-6 py-5 dark:border-slate-700">
        <div className="flex flex-col gap-4">
          <Field
            label="Participants filter (optional)"
            help={
              <HelpPopover fieldLabel="Participants filter" testId="help-prep-participants">
                <p>Comma-separated list of participant handles.</p>
                <p className="mt-1">Leave blank to prep every named entry in the manifest.</p>
              </HelpPopover>
            }
          >
            <input
              type="text"
              value={participants}
              onChange={(e) => setParticipants(e.target.value)}
              className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              placeholder="alice, bob, carol"
              data-testid="prep-assets-participants"
            />
          </Field>

          <button
            type="button"
            onClick={() => setAdvancedOpen((v) => !v)}
            className="flex items-center gap-1 text-xs text-neutral-500 hover:text-neutral-700 dark:text-slate-400 dark:hover:text-slate-200"
            data-testid="prep-assets-advanced-toggle"
          >
            {advancedOpen ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
            Advanced
          </button>
          {advancedOpen && (
            <div className="grid grid-cols-1 gap-3 rounded-md border border-neutral-200 bg-neutral-50 p-3 text-xs dark:border-slate-700 dark:bg-slate-900/30">
              <Field label="Manifest path">
                <input
                  type="text"
                  value={manifestPath}
                  onChange={(e) => setManifestPath(e.target.value)}
                  placeholder="bot/conversation/manifest.yaml"
                  className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                  data-testid="prep-assets-manifest-path"
                />
              </Field>
              <Field label="Costume source dir">
                <input
                  type="text"
                  value={costumeSource}
                  onChange={(e) => setCostumeSource(e.target.value)}
                  placeholder="bot/assets/costumes"
                  className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                  data-testid="prep-assets-costume-source"
                />
              </Field>
              <Field label="Output dir">
                <input
                  type="text"
                  value={outputDir}
                  onChange={(e) => setOutputDir(e.target.value)}
                  placeholder="e2e/bots-app/run"
                  className="w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                  data-testid="prep-assets-output-dir"
                />
              </Field>
            </div>
          )}

          <div className="flex items-center justify-end">
            <button
              type="button"
              onClick={handleStart}
              disabled={startMutation.isPending || activeJobId !== null}
              className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600"
              data-testid="prep-assets-run"
            >
              {startMutation.isPending ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <Play className="h-3.5 w-3.5" />
              )}
              {startMutation.isPending
                ? "Starting…"
                : activeJobId !== null
                  ? "Job running…"
                  : "Run prep-assets"}
            </button>
          </div>
        </div>
      </div>

      {activeJobId && (
        <PrepAssetsLogDialog
          jobId={activeJobId}
          onClose={(finalStatus) => {
            setActiveJobId(null);
            if (finalStatus === "done") {
              onToast({
                title: "prep-assets done",
                description: "Costume + audio files regenerated.",
                variant: "success",
              });
            } else if (finalStatus === "failed") {
              onToast({
                title: "prep-assets failed",
                description: "Check the streamed log for details.",
                variant: "error",
              });
            }
          }}
        />
      )}
    </section>
  );
}

interface PrepAssetsLogDialogProps {
  jobId: string;
  onClose: (finalStatus: "done" | "failed" | "running") => void;
}

/**
 * Live-streaming log modal. Subscribes to the SSE endpoint and
 * appends each line into a scrollable monospace pane. Polls the
 * status endpoint as a fallback so the final-summary (audioPrepped /
 * costumesPrepped counts) becomes available even if the SSE pipe
 * terminated abruptly.
 */
function PrepAssetsLogDialog({ jobId, onClose }: PrepAssetsLogDialogProps) {
  const [lines, setLines] = useState<string[]>([]);
  const [finalStatus, setFinalStatus] = useState<"done" | "failed" | "running">("running");
  const logRef = useRef<HTMLPreElement | null>(null);

  // Backup status poll so the final summary materialises even if SSE
  // dropped the `end` event.
  const statusQuery = useQuery({
    queryKey: ["prep-assets", "status", jobId],
    queryFn: () => api.prepAssetsStatus(jobId),
    refetchInterval: (q) => {
      const data = q.state.data as PrepAssetsJobStatus | undefined;
      return data?.status === "running" ? 2_000 : false;
    },
    enabled: true,
  });

  useEffect(() => {
    const es = new EventSource(`/api/assets/prep/${encodeURIComponent(jobId)}/stream`);
    const onMessage = (e: MessageEvent) => {
      setLines((prev) => [...prev, e.data as string]);
      // Auto-scroll the log pane.
      requestAnimationFrame(() => {
        if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
      });
    };
    const onEnd = (e: MessageEvent) => {
      try {
        const parsed = JSON.parse(e.data as string) as { status: string };
        if (parsed.status === "done" || parsed.status === "failed") {
          setFinalStatus(parsed.status);
        }
      } catch {
        // ignore
      }
      es.close();
    };
    es.addEventListener("message", onMessage);
    es.addEventListener("end", onEnd as EventListener);
    es.onerror = () => {
      es.close();
    };
    return () => {
      es.close();
    };
  }, [jobId]);

  // Reconcile final status with the fallback poll (e.g. if SSE silently
  // closed without firing `end`).
  useEffect(() => {
    const s = statusQuery.data?.status;
    if (s === "done" || s === "failed") {
      setFinalStatus(s);
    }
  }, [statusQuery.data?.status]);

  return (
    <Dialog.Root open={true} onOpenChange={(o) => (!o ? onClose(finalStatus) : null)}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(94vw,720px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800"
          data-testid="prep-assets-log-dialog"
        >
          <div className="flex items-center justify-between border-b border-neutral-200 px-5 py-3 dark:border-slate-700">
            <div>
              <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
                prep-assets log
              </Dialog.Title>
              <Dialog.Description className="text-xs text-neutral-500 dark:text-slate-400">
                Job <code className="font-mono">{jobId.slice(0, 8)}</code>
                {" — "}
                <StatusPill status={finalStatus} />
              </Dialog.Description>
            </div>
            <Dialog.Close
              className="rounded p-1 text-neutral-400 hover:bg-neutral-100 dark:text-slate-500 dark:hover:bg-slate-700"
              aria-label="Close"
            >
              <X className="h-4 w-4" />
            </Dialog.Close>
          </div>
          <pre
            ref={logRef}
            className="max-h-[min(60vh,480px)] overflow-y-auto bg-neutral-950 p-4 font-mono text-xs leading-relaxed text-emerald-200"
            data-testid="prep-assets-log"
          >
            {lines.length === 0 ? (
              <span className="text-neutral-400">Waiting for output…</span>
            ) : (
              lines.join("\n")
            )}
          </pre>
          <div className="flex items-center justify-between gap-2 border-t border-neutral-200 px-5 py-3 dark:border-slate-700">
            <p className="text-[11px] text-neutral-500 dark:text-slate-400">
              Closing this window does not kill the job — ffmpeg keeps running in the
              background.
            </p>
            <button
              type="button"
              onClick={() => onClose(finalStatus)}
              className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
              data-testid="prep-assets-close"
            >
              Close
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function StatusPill({ status }: { status: "running" | "done" | "failed" }) {
  if (status === "done") {
    return (
      <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-[11px] font-medium text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-200">
        done
      </span>
    );
  }
  if (status === "failed") {
    return (
      <span className="rounded-full bg-red-100 px-2 py-0.5 text-[11px] font-medium text-red-700 dark:bg-red-900/40 dark:text-red-200">
        failed
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-amber-100 px-2 py-0.5 text-[11px] font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-200">
      <Loader2 className="h-3 w-3 animate-spin" />
      running
    </span>
  );
}

interface FieldProps {
  label: string;
  help?: React.ReactNode;
  children: React.ReactNode;
}

function Field({ label, help, children }: FieldProps) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center gap-1.5">
        <label className="text-sm font-medium text-neutral-800 dark:text-slate-200">
          {label}
        </label>
        {help}
      </div>
      {children}
    </div>
  );
}
