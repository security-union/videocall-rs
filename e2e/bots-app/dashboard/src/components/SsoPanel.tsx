import { useCallback, useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import * as Dialog from "@radix-ui/react-dialog";
import { CircleDot, Loader2, ShieldCheck, ShieldAlert, X } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type {
  SsoRecaptureStartResponse,
  SsoStatusResponse,
  VpnStatusResponse,
} from "../api/types";

/**
 * Polling cadences. The VPN check is light (one server-side `fetch`),
 * so 30s gives the operator a fresh signal without hammering the host.
 * SSO state changes only via the recapture flow + filesystem, so 60s
 * is more than enough. Both queries also refetch immediately when the
 * panel opens (via `enabled` toggling) to give the operator current
 * data on first view.
 */
export const VPN_POLL_INTERVAL_MS = 30_000;
export const SSO_POLL_INTERVAL_MS = 60_000;

interface SsoPanelProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onToast?: (t: { title: string; description?: string; variant: "success" | "error" | "info" }) => void;
}

/**
 * Dialog surfacing HCL VPN reachability + SSO storage-state status,
 * with a re-capture flow that drives the server-side headed-Chrome
 * session.
 *
 * UX timing:
 *   1. Operator opens the dialog → both queries refetch immediately.
 *   2. Operator clicks "Re-capture SSO state" → POST /api/sso/recapture
 *      runs server-side, which spawns headed Chrome and stashes a
 *      session handle keyed by uuid. The button switches to a pending
 *      state and the dialog renders the "Chrome opened" prompt.
 *   3. Operator completes the HCL SSO challenge in the newly opened
 *      Chrome window (the dashboard cannot observe this directly).
 *   4. Operator clicks "I'm logged in, save" → POST .../complete
 *      asks the server to call `context.storageState()` and tear
 *      Chrome down; the response is the fresh SSO status, which
 *      replaces the in-panel display.
 *   5. If the operator closes the Chrome window before clicking save,
 *      the server-side idle timer (15 min by default) auto-cancels
 *      the session and tears the dead context down. Clicking "I'm
 *      logged in, save" after that returns a 404 + the panel surfaces
 *      "session expired — start over".
 */
export function SsoPanel({ open, onOpenChange, onToast }: SsoPanelProps) {
  const queryClient = useQueryClient();
  const [activeSession, setActiveSession] = useState<SsoRecaptureStartResponse | null>(null);

  const vpnQuery = useQuery({
    queryKey: ["sso", "vpn-status"],
    queryFn: api.vpnStatus,
    refetchInterval: VPN_POLL_INTERVAL_MS,
    enabled: open,
  });
  const ssoQuery = useQuery({
    queryKey: ["sso", "status"],
    queryFn: api.ssoStatus,
    refetchInterval: SSO_POLL_INTERVAL_MS,
    enabled: open,
  });

  // When the panel opens, force a refresh so the operator sees the
  // current state without waiting for the next poll tick.
  useEffect(() => {
    if (open) {
      queryClient.invalidateQueries({ queryKey: ["sso", "vpn-status"] });
      queryClient.invalidateQueries({ queryKey: ["sso", "status"] });
    }
  }, [open, queryClient]);

  const startMutation = useMutation({
    mutationFn: () => api.ssoRecaptureStart({}),
    onSuccess: (data) => {
      setActiveSession(data);
      onToast?.({
        title: "Chrome opened",
        description: "Complete the HCL SSO login, then click 'I'm logged in, save'.",
        variant: "info",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast?.({ title: "Recapture failed to start", description: msg, variant: "error" });
    },
  });
  const completeMutation = useMutation({
    mutationFn: (sessionId: string) => api.ssoRecaptureComplete(sessionId),
    onSuccess: () => {
      setActiveSession(null);
      queryClient.invalidateQueries({ queryKey: ["sso", "status"] });
      onToast?.({
        title: "SSO state saved",
        description: "Future launches will reuse this captured session.",
        variant: "success",
      });
    },
    onError: (err) => {
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      // 404 here means the server-side session was idle-timed-out or
      // already cancelled. Reset so the operator can try again.
      if (err instanceof DashboardApiError && err.status === 404) {
        setActiveSession(null);
      }
      onToast?.({ title: "Save failed", description: msg, variant: "error" });
    },
  });
  const cancelMutation = useMutation({
    mutationFn: (sessionId: string) => api.ssoRecaptureCancel(sessionId),
    onSuccess: () => {
      setActiveSession(null);
      onToast?.({
        title: "Recapture cancelled",
        description: "The headed Chrome was closed without saving.",
        variant: "info",
      });
    },
    onError: (err) => {
      // Cancel best-effort. Surface but don't block the UI from
      // forgetting the session.
      setActiveSession(null);
      const msg = err instanceof DashboardApiError ? err.message : (err as Error).message;
      onToast?.({ title: "Cancel failed", description: msg, variant: "error" });
    },
  });

  const handleStart = useCallback(() => startMutation.mutate(), [startMutation]);
  const handleComplete = useCallback(() => {
    if (!activeSession) return;
    completeMutation.mutate(activeSession.recaptureSessionId);
  }, [activeSession, completeMutation]);
  const handleCancel = useCallback(() => {
    if (!activeSession) return;
    cancelMutation.mutate(activeSession.recaptureSessionId);
  }, [activeSession, cancelMutation]);

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-neutral-900/40 backdrop-blur-sm dark:bg-black/60" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(92vw,560px)] -translate-x-1/2 -translate-y-1/2 rounded-lg border border-neutral-200 bg-white p-6 shadow-xl focus:outline-none dark:border-slate-700 dark:bg-slate-800"
          data-testid="sso-panel"
        >
          <div className="flex items-start justify-between">
            <div>
              <Dialog.Title className="text-base font-semibold text-neutral-900 dark:text-slate-100">
                HCL VPN &amp; SSO
              </Dialog.Title>
              <Dialog.Description className="mt-1 text-xs text-neutral-500 dark:text-slate-400">
                VPN reachability and the captured SSO state file the bots use to
                pass HCL SSO without an interactive login each run.
              </Dialog.Description>
            </div>
            <Dialog.Close className="rounded p-1 text-neutral-400 hover:bg-neutral-100 dark:text-slate-500 dark:hover:bg-slate-700">
              <X className="h-4 w-4" />
            </Dialog.Close>
          </div>

          <div className="mt-4 space-y-4">
            <VpnSection data={vpnQuery.data} isLoading={vpnQuery.isLoading} />
            <SsoSection
              data={ssoQuery.data}
              isLoading={ssoQuery.isLoading}
              activeSession={activeSession}
              onStart={handleStart}
              onComplete={handleComplete}
              onCancel={handleCancel}
              starting={startMutation.isPending}
              saving={completeMutation.isPending}
              cancelling={cancelMutation.isPending}
            />
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function VpnSection({ data, isLoading }: { data?: VpnStatusResponse; isLoading: boolean }) {
  const up = data?.status === "up";
  return (
    <section
      className="rounded-md border border-neutral-200 bg-neutral-50/40 p-3 dark:border-slate-700 dark:bg-slate-900/30"
      data-testid="sso-panel-vpn"
    >
      <h3 className="flex items-center gap-2 text-sm font-semibold text-neutral-800 dark:text-slate-100">
        {isLoading ? (
          <Loader2 className="h-4 w-4 animate-spin text-neutral-400" aria-hidden="true" />
        ) : up ? (
          <ShieldCheck className="h-4 w-4 text-emerald-500" aria-hidden="true" />
        ) : (
          <ShieldAlert className="h-4 w-4 text-red-500" aria-hidden="true" />
        )}
        HCL VPN
      </h3>
      <p className="mt-1 text-xs text-neutral-600 dark:text-slate-300">
        {isLoading
          ? "Checking…"
          : up
            ? `Reachable (${(data as { responseTimeMs?: number }).responseTimeMs ?? "?"} ms response time)`
            : `Unreachable: ${(data as { error?: string } | undefined)?.error ?? "unknown"}. Connect HCL VPN to continue.`}
      </p>
    </section>
  );
}

interface SsoSectionProps {
  data?: SsoStatusResponse;
  isLoading: boolean;
  activeSession: SsoRecaptureStartResponse | null;
  onStart: () => void;
  onComplete: () => void;
  onCancel: () => void;
  starting: boolean;
  saving: boolean;
  cancelling: boolean;
}

function SsoSection({
  data,
  isLoading,
  activeSession,
  onStart,
  onComplete,
  onCancel,
  starting,
  saving,
  cancelling,
}: SsoSectionProps) {
  const ageHours = data?.ageHours ?? null;
  const tone = useMemo(() => deriveSsoTone(data), [data]);
  return (
    <section
      className="rounded-md border border-neutral-200 bg-neutral-50/40 p-3 dark:border-slate-700 dark:bg-slate-900/30"
      data-testid="sso-panel-state"
    >
      <h3 className="flex items-center gap-2 text-sm font-semibold text-neutral-800 dark:text-slate-100">
        <CircleDot className={`h-3 w-3 ${TONE_DOT[tone]}`} aria-hidden="true" />
        SSO storage state
      </h3>
      {isLoading ? (
        <p className="mt-1 text-xs text-neutral-500 dark:text-slate-400">Reading…</p>
      ) : data === undefined ? (
        <p className="mt-1 text-xs text-neutral-500 dark:text-slate-400">Status unavailable.</p>
      ) : data.exists ? (
        <dl className="mt-2 grid grid-cols-[6rem_1fr] gap-x-3 gap-y-1 text-xs">
          <dt className="text-neutral-500 dark:text-slate-400">File</dt>
          <dd className="font-mono text-neutral-700 dark:text-slate-200" data-testid="sso-file-path">
            {data.filePath}
          </dd>
          <dt className="text-neutral-500 dark:text-slate-400">Age</dt>
          <dd className="text-neutral-700 dark:text-slate-200" data-testid="sso-age">
            {ageHours !== null ? `${ageHours.toFixed(1)}h` : "—"}
          </dd>
          <dt className="text-neutral-500 dark:text-slate-400">Size</dt>
          <dd className="text-neutral-700 dark:text-slate-200">
            {data.size !== null ? `${data.size} bytes` : "—"}
          </dd>
        </dl>
      ) : (
        <p
          className="mt-1 text-xs text-red-600 dark:text-red-400"
          data-testid="sso-missing"
        >
          No SSO state captured yet. Bots will hit the HCL SSO portal on every launch
          until you capture one.
        </p>
      )}

      {activeSession === null ? (
        <button
          type="button"
          onClick={onStart}
          disabled={starting}
          className="mt-3 inline-flex items-center gap-2 rounded-md bg-sky-500 px-3 py-1.5 text-sm font-medium text-white shadow-sm hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-neutral-300 dark:disabled:bg-slate-600"
          data-testid="sso-recapture-start"
        >
          {starting ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
              Opening Chrome…
            </>
          ) : data?.exists ? (
            "Re-capture SSO state"
          ) : (
            "Capture SSO state"
          )}
        </button>
      ) : (
        <div
          className="mt-3 rounded-md border border-amber-200 bg-amber-50 p-3 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-900/20 dark:text-amber-200"
          data-testid="sso-recapture-active"
        >
          <p className="font-medium">Chrome opened.</p>
          <p className="mt-1">
            Complete the HCL SSO login in that window. Once you&apos;re back at the videocall
            app, click <strong>I&apos;m logged in, save</strong> below. Closing the Chrome
            window without saving will trigger an auto-cancel after the idle timeout.
          </p>
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              onClick={onComplete}
              disabled={saving || cancelling}
              className="rounded-md bg-emerald-500 px-3 py-1.5 text-xs font-medium text-white shadow-sm hover:bg-emerald-600 disabled:cursor-not-allowed disabled:bg-neutral-300"
              data-testid="sso-recapture-complete"
            >
              {saving ? "Saving…" : "I'm logged in, save"}
            </button>
            <button
              type="button"
              onClick={onCancel}
              disabled={saving || cancelling}
              className="rounded-md border border-neutral-300 bg-white px-3 py-1.5 text-xs font-medium text-neutral-700 hover:bg-neutral-50 disabled:cursor-not-allowed dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
              data-testid="sso-recapture-cancel"
            >
              {cancelling ? "Cancelling…" : "Cancel"}
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

type SsoTone = "green" | "yellow" | "red";

const TONE_DOT: Record<SsoTone, string> = {
  green: "text-emerald-500",
  yellow: "text-amber-500",
  red: "text-red-500",
};

/**
 * Color-coding rule for the SSO chip + panel header:
 *   - missing → red
 *   - older than 12h → yellow (SSO sessions usually expire on this
 *     order of magnitude; we don't try to parse cookie max-age)
 *   - otherwise → green
 *
 * Exported for unit testing.
 */
export function deriveSsoTone(data?: SsoStatusResponse): SsoTone {
  if (data === undefined || !data.exists) return "red";
  if (data.ageHours !== null && data.ageHours > 12) return "yellow";
  return "green";
}
