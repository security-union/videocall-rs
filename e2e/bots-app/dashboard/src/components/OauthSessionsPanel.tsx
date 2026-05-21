import { useCallback, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { KeyRound, Loader2, Plus, Trash2, X } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type {
  OauthCaptureStartResponse,
  OauthSessionInfo,
  OauthSessionsResponse,
} from "../api/types";
import type { ToastEntry } from "./ToastShelf";
import { ConfirmDialog } from "./ConfirmDialog";
import { HelpPopover } from "./ui/HelpPopover";

/**
 * Server-side regex for the OAuth session label. Mirrored from
 * `OAUTH_LABEL_PATTERN` in `e2e/bots-app/src/control/server.ts` —
 * authoritative validation runs on the server; this duplicate just
 * surfaces feedback before the network round-trip.
 */
const LABEL_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._@+-]{0,127}$/;
const DEFAULT_START_URL = "https://app.videocall.rs/";

interface OauthSessionsPanelProps {
  onToast: (t: Omit<ToastEntry, "id">) => void;
}

/**
 * "OAuth Sessions" card for the Tools page. Lists captured per-account
 * storage-state files (sibling of the HCL SSO recapture, but for
 * arbitrary OAuth providers like Google for app.videocall.rs).
 * Capturing a new session spawns a headed Chrome via Playwright, the
 * same way the SSO flow does.
 */
export function OauthSessionsPanel({ onToast }: OauthSessionsPanelProps) {
  const qc = useQueryClient();
  const sessionsQuery = useQuery({
    queryKey: ["oauth", "sessions"],
    queryFn: api.oauthSessions,
    refetchInterval: 60_000,
  });
  const [captureOpen, setCaptureOpen] = useState(false);
  const [activeCapture, setActiveCapture] = useState<{
    response: OauthCaptureStartResponse;
    label: string;
  } | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<OauthSessionInfo | null>(null);

  const refresh = useCallback(
    () => qc.invalidateQueries({ queryKey: ["oauth", "sessions"] }),
    [qc],
  );

  const startMutation = useMutation({
    mutationFn: (input: { label: string; startUrl: string }) =>
      api.oauthCaptureStart({ label: input.label, startUrl: input.startUrl }),
    onSuccess: (data, vars) => {
      setActiveCapture({ response: data, label: vars.label });
      setCaptureOpen(false);
      onToast({
        title: "Chrome opened",
        description: `Complete the OAuth login for "${vars.label}", then click 'Save'.`,
        variant: "info",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Capture failed to start", description: msg, variant: "error" });
    },
  });

  const completeMutation = useMutation({
    mutationFn: (sessionId: string) => api.oauthCaptureComplete(sessionId),
    onSuccess: (info) => {
      setActiveCapture(null);
      refresh();
      onToast({
        title: "OAuth session saved",
        description: `Captured "${info.label}" at ${info.filePath}`,
        variant: "success",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      if (err instanceof DashboardApiError && err.status === 404) {
        setActiveCapture(null);
      }
      onToast({ title: "Save failed", description: msg, variant: "error" });
    },
  });

  const cancelMutation = useMutation({
    mutationFn: (sessionId: string) => api.oauthCaptureCancel(sessionId),
    onSuccess: () => {
      setActiveCapture(null);
      onToast({
        title: "Capture cancelled",
        description: "The headed Chrome was closed without saving.",
        variant: "info",
      });
    },
    onError: (err) => {
      setActiveCapture(null);
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast({ title: "Cancel failed", description: msg, variant: "error" });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (label: string) => api.oauthSessionDelete(label),
    onSuccess: (data) => {
      setConfirmDelete(null);
      refresh();
      onToast({
        title: "Session deleted",
        description: `Removed "${data.label}".`,
        variant: "success",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      setConfirmDelete(null);
      onToast({ title: "Delete failed", description: msg, variant: "error" });
    },
  });

  const sessions = (sessionsQuery.data as OauthSessionsResponse | undefined)?.sessions ?? [];

  return (
    <section
      className="rounded-lg border border-neutral-200 bg-white shadow-sm dark:border-slate-700 dark:bg-slate-800"
      data-testid="oauth-sessions-section"
    >
      <div className="flex items-center justify-between px-6 py-4">
        <div className="flex items-center gap-2">
          <KeyRound className="h-5 w-5 text-sky-500" aria-hidden="true" />
          <div>
            <h2 className="text-lg font-semibold tracking-tight text-neutral-900 dark:text-slate-100">
              OAuth Sessions
            </h2>
            <p className="text-sm text-neutral-500 dark:text-slate-400">
              Captured per-account storage-state files used by{" "}
              <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                --auth=storage-state
              </code>{" "}
              against real-OAuth targets like{" "}
              <code className="rounded bg-neutral-100 px-1 py-0.5 font-mono text-[11px] dark:bg-slate-900">
                app.videocall.rs
              </code>
              .
            </p>
          </div>
          <HelpPopover fieldLabel="OAuth sessions" testId="help-oauth-sessions">
            <p>
              Each session is a saved Playwright storage-state JSON that captures the cookies +
              localStorage of a real Google OAuth login.
            </p>
            <p className="mt-1">
              Mirrors{" "}
              <code className="font-mono text-[11px]">bots-app login &lt;account&gt;</code> from
              the CLI.
            </p>
            <p className="mt-1">
              The HCL SSO state (hcl-sso.json) is managed separately — see the SSO chip in the
              header.
            </p>
          </HelpPopover>
        </div>
        <button
          type="button"
          onClick={() => setCaptureOpen(true)}
          disabled={activeCapture !== null}
          className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600"
          data-testid="oauth-capture-new"
        >
          <Plus className="h-4 w-4" />
          Capture new
        </button>
      </div>
      <div className="border-t border-neutral-200 dark:border-slate-700">
        {sessionsQuery.isLoading ? (
          <p className="px-6 py-4 text-sm text-neutral-500 dark:text-slate-400">Loading…</p>
        ) : sessions.length === 0 ? (
          <p
            className="px-6 py-4 text-sm text-neutral-500 dark:text-slate-400"
            data-testid="oauth-sessions-empty"
          >
            No OAuth sessions captured yet. Click <strong>Capture new</strong> to record one for
            a Google-authenticated account.
          </p>
        ) : (
          <ul className="divide-y divide-neutral-100 dark:divide-slate-700">
            {sessions.map((s) => (
              <li
                key={s.label}
                className="flex items-center justify-between gap-4 px-6 py-3 text-sm"
                data-testid={`oauth-session-${s.label}`}
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span
                      className="truncate font-medium text-neutral-900 dark:text-slate-100"
                      data-testid={`oauth-session-label-${s.label}`}
                    >
                      {s.label}
                    </span>
                    <span className="text-xs text-neutral-500 dark:text-slate-400">
                      ({s.size} bytes)
                    </span>
                  </div>
                  <div className="mt-1 flex flex-wrap items-center gap-3 text-xs text-neutral-500 dark:text-slate-400">
                    <code className="truncate font-mono text-[11px] text-neutral-600 dark:text-slate-300">
                      {s.filePath}
                    </code>
                    <span>captured {s.ageHours.toFixed(1)}h ago</span>
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <button
                    type="button"
                    onClick={() => setCaptureOpen(true)}
                    className="rounded-md border border-neutral-300 px-2.5 py-1 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                    data-testid={`oauth-session-recapture-${s.label}`}
                  >
                    Re-capture
                  </button>
                  <button
                    type="button"
                    onClick={() => setConfirmDelete(s)}
                    className="inline-flex items-center gap-1 rounded-md border border-red-200 px-2.5 py-1 text-xs text-red-700 hover:bg-red-50 dark:border-red-800 dark:text-red-300 dark:hover:bg-red-900/30"
                    data-testid={`oauth-session-delete-${s.label}`}
                  >
                    <Trash2 className="h-3 w-3" />
                    Delete
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>

      <CaptureDialog
        open={captureOpen}
        onOpenChange={setCaptureOpen}
        onSubmit={(label, startUrl) => startMutation.mutate({ label, startUrl })}
        starting={startMutation.isPending}
      />

      <ActiveCaptureDialog
        active={activeCapture}
        onComplete={() => {
          if (activeCapture) completeMutation.mutate(activeCapture.response.captureSessionId);
        }}
        onCancel={() => {
          if (activeCapture) cancelMutation.mutate(activeCapture.response.captureSessionId);
        }}
        saving={completeMutation.isPending}
        cancelling={cancelMutation.isPending}
      />

      {confirmDelete && (
        <ConfirmDialog
          open={true}
          title="Delete OAuth session?"
          body={`The captured session "${confirmDelete.label}" will be removed from disk. This cannot be undone.`}
          confirmLabel={deleteMutation.isPending ? "Deleting…" : "Delete"}
          destructive
          onCancel={() => setConfirmDelete(null)}
          onConfirm={() => deleteMutation.mutate(confirmDelete.label)}
        />
      )}
    </section>
  );
}

interface CaptureDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSubmit: (label: string, startUrl: string) => void;
  starting: boolean;
}

function CaptureDialog({ open, onOpenChange, onSubmit, starting }: CaptureDialogProps) {
  const [label, setLabel] = useState("");
  const [startUrl, setStartUrl] = useState(DEFAULT_START_URL);
  const [error, setError] = useState<string | null>(null);

  const handleSubmit: React.FormEventHandler<HTMLFormElement> = (e) => {
    e.preventDefault();
    if (label.trim() === "") {
      setError("Label is required");
      return;
    }
    if (!LABEL_PATTERN.test(label.trim())) {
      setError("Label must match alphanumerics, '.', '_', '@', '+', '-'");
      return;
    }
    setError(null);
    onSubmit(label.trim(), startUrl.trim() || DEFAULT_START_URL);
    setLabel("");
    setStartUrl(DEFAULT_START_URL);
  };

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(92vw,520px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-6 shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800"
          data-testid="oauth-capture-dialog"
        >
          <div className="flex items-start justify-between">
            <div>
              <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
                Capture OAuth session
              </Dialog.Title>
              <Dialog.Description className="mt-1 text-xs text-neutral-500 dark:text-slate-400">
                Opens a headed Chrome at the start URL. Complete the login normally, then
                click <strong>Save</strong>.
              </Dialog.Description>
            </div>
            <Dialog.Close className="rounded p-1 text-neutral-400 hover:bg-neutral-100 dark:text-slate-500 dark:hover:bg-slate-700">
              <X className="h-4 w-4" />
            </Dialog.Close>
          </div>

          <form className="mt-4 flex flex-col gap-4" onSubmit={handleSubmit}>
            <div className="flex flex-col gap-1.5">
              <label
                htmlFor="oauth-label"
                className="text-sm font-medium text-neutral-800 dark:text-slate-200"
              >
                Label <span className="text-red-500">*</span>
              </label>
              <input
                id="oauth-label"
                type="text"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                placeholder="alice"
                className="rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                data-testid="oauth-capture-label"
                autoFocus
              />
              <p className="text-xs text-neutral-500 dark:text-slate-400">
                Saved as{" "}
                <code className="font-mono text-[11px]">
                  &lt;runDir&gt;/auth/{label || "&lt;label&gt;"}.json
                </code>
              </p>
            </div>

            <div className="flex flex-col gap-1.5">
              <label
                htmlFor="oauth-start-url"
                className="text-sm font-medium text-neutral-800 dark:text-slate-200"
              >
                Start URL
              </label>
              <input
                id="oauth-start-url"
                type="url"
                value={startUrl}
                onChange={(e) => setStartUrl(e.target.value)}
                placeholder={DEFAULT_START_URL}
                className="rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm shadow-sm focus:border-sky-500 focus:outline-none focus:ring-1 focus:ring-sky-500 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                data-testid="oauth-capture-start-url"
              />
            </div>

            {error && (
              <p className="text-xs text-red-600 dark:text-red-400" role="alert">
                {error}
              </p>
            )}

            <div className="flex justify-end gap-2">
              <Dialog.Close asChild>
                <button
                  type="button"
                  className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-700"
                >
                  Cancel
                </button>
              </Dialog.Close>
              <button
                type="submit"
                disabled={starting}
                className="inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600"
                data-testid="oauth-capture-submit"
              >
                {starting ? (
                  <>
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    Opening Chrome…
                  </>
                ) : (
                  "Open Chrome"
                )}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

interface ActiveCaptureDialogProps {
  active: { response: OauthCaptureStartResponse; label: string } | null;
  onComplete: () => void;
  onCancel: () => void;
  saving: boolean;
  cancelling: boolean;
}

function ActiveCaptureDialog({
  active,
  onComplete,
  onCancel,
  saving,
  cancelling,
}: ActiveCaptureDialogProps) {
  return (
    <Dialog.Root open={active !== null}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(92vw,520px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-amber-200 bg-amber-50 p-6 shadow-xl focus:outline-none dark:border-amber-700 dark:bg-amber-900/30"
          data-testid="oauth-capture-active-dialog"
        >
          <Dialog.Title className="text-base font-semibold text-amber-900 dark:text-amber-100">
            Chrome opened — complete the login
          </Dialog.Title>
          <Dialog.Description className="mt-2 text-sm text-amber-800 dark:text-amber-200">
            A headed Chrome window has opened
            {active && (
              <>
                {" "}
                at <code className="font-mono text-[11px]">{active.response.startUrl}</code>.
              </>
            )}{" "}
            Complete the OAuth login in that window. When you&apos;re back at the videocall
            app, click <strong>Save</strong> below.
          </Dialog.Description>
          {active && (
            <p className="mt-2 text-xs text-amber-700 dark:text-amber-300">
              Label: <code className="font-mono text-[11px]">{active.label}</code>
            </p>
          )}
          <div className="mt-4 flex justify-end gap-2">
            <button
              type="button"
              onClick={onCancel}
              disabled={saving || cancelling}
              className="rounded-md border border-amber-300 bg-white px-3 py-1.5 text-sm font-medium text-amber-900 hover:bg-amber-100 disabled:cursor-not-allowed dark:border-amber-700 dark:bg-amber-900/40 dark:text-amber-100 dark:hover:bg-amber-900/60"
              data-testid="oauth-capture-active-cancel"
            >
              {cancelling ? "Cancelling…" : "Cancel"}
            </button>
            <button
              type="button"
              onClick={onComplete}
              disabled={saving || cancelling}
              className="rounded-md bg-emerald-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-emerald-600 disabled:cursor-not-allowed disabled:bg-neutral-300"
              data-testid="oauth-capture-active-save"
            >
              {saving ? "Saving…" : "Save"}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
