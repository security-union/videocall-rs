import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check, ChevronDown, ChevronRight, Copy, Terminal } from "lucide-react";

import { api, DashboardApiError } from "../api/client";
import type { SshPreviewLaunchRequest } from "../api/types";
import { HelpPopover } from "./ui/HelpPopover";

/**
 * Subset of the launch spec the preview surface cares about. The
 * dashboard's LaunchForm / MultiLaunchForm both stash a wider state
 * object — they pass only the fields that actually shape the SSH
 * command into this component so changes in unrelated fields (e.g.
 * costume / audio) do NOT thrash the preview's React Query cache.
 */
export type SshPreviewSpec = SshPreviewLaunchRequest;

interface SshCommandPreviewProps {
  /**
   * Label of the registered SSH host the bot will run on. Pass `null`
   * to suppress the entire preview (e.g. when the operator hasn't
   * picked "SSH-able host" yet or hasn't selected a host from the
   * dropdown). When `null` the component returns nothing — no card,
   * no skeleton, no query.
   */
  hostLabel: string | null;
  /** Launch spec the preview should render against. */
  spec: SshPreviewSpec;
  /**
   * Optional subtitle rendered under the header. Used by the
   * multi-launch flow to surface the "preview for first participant"
   * caveat ("all N bots use the same host"). Single-launch flows
   * leave it unset.
   */
  subtitle?: string;
  /** data-testid prefix used by the React tests. */
  testIdPrefix?: string;
}

/**
 * Debounce window (ms) applied to the `spec` value before the preview
 * issues a fetch. Long enough to absorb keystroke-by-keystroke edits
 * to the participant / ttl / displayName / meetingURL inputs; short
 * enough that the preview reflects the operator's latest pick once
 * they pause.
 */
const PREVIEW_DEBOUNCE_MS = 250;

/**
 * Collapsible card that shows the exact `ssh` command the dashboard
 * will execute when the operator clicks Launch. Backed by
 * `POST /api/hosts/:label/preview-launch` — the endpoint never spawns
 * anything; it just constructs the argv server-side and returns the
 * human-readable rendering.
 *
 * UX:
 *   - Default-collapsed so the form's main flow is not cluttered.
 *   - Copy button writes the `display` field to the clipboard.
 *   - Errors (e.g. invalid spec) render inline.
 *   - When `hostLabel === null`, renders nothing.
 */
export function SshCommandPreview({
  hostLabel,
  spec,
  subtitle,
  testIdPrefix = "ssh-cmd-preview",
}: SshCommandPreviewProps) {
  const [open, setOpen] = useState<boolean>(false);
  // The spec we actually run the query against — bumped from the props
  // after the debounce window expires.
  const [debouncedSpec, setDebouncedSpec] = useState<SshPreviewSpec>(spec);
  const [copied, setCopied] = useState<boolean>(false);

  // Debounce the incoming spec. setTimeout instead of useDeferredValue
  // because we want a strict time-based gate (not React's
  // priority-based one) — the gate keeps the query key stable enough
  // that TanStack Query does not double-fire for every keystroke.
  useEffect(() => {
    const t = setTimeout(() => setDebouncedSpec(spec), PREVIEW_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [spec]);

  // Reset the "copied" affordance whenever the underlying preview text
  // changes — keeps the green check from sticking after the operator
  // edits the form post-copy.
  useEffect(() => {
    setCopied(false);
  }, [debouncedSpec, hostLabel]);

  const enabled = hostLabel !== null && hostLabel !== "" && open;
  const previewQuery = useQuery({
    queryKey: ["ssh", "preview", hostLabel, debouncedSpec],
    queryFn: () => {
      // The `enabled` gate below ensures this is unreachable when
      // hostLabel is null; the non-null assertion is just for TS.
      return api.previewSshLaunch(hostLabel as string, debouncedSpec);
    },
    enabled,
    // Stale-while-revalidate is the wrong shape here — the preview
    // should always reflect the latest spec, not a cached older one.
    staleTime: 0,
    retry: false,
  });

  if (hostLabel === null || hostLabel === "") return null;

  const display = previewQuery.data?.display ?? "";
  const errorMsg =
    previewQuery.error instanceof DashboardApiError
      ? previewQuery.error.message
      : previewQuery.error
        ? (previewQuery.error as Error).message
        : null;

  const handleCopy = async (): Promise<void> => {
    if (!display) return;
    try {
      await navigator.clipboard.writeText(display);
      setCopied(true);
      // Auto-clear the check after 2s so a second click feels natural.
      setTimeout(() => setCopied(false), 2_000);
    } catch {
      // Clipboard rejections are rare in production (HTTPS + secure
      // contexts on the dashboard) but still possible in some embed
      // scenarios; we deliberately do nothing — the operator can
      // re-select the text manually.
    }
  };

  return (
    <div
      className="rounded-lg border border-neutral-200 bg-neutral-50/40 p-3 text-sm dark:border-slate-700 dark:bg-slate-900/30"
      data-testid={`${testIdPrefix}-root`}
    >
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 text-left text-sm font-medium text-neutral-800 dark:text-slate-200"
        data-testid={`${testIdPrefix}-toggle`}
        aria-expanded={open}
      >
        {open ? (
          <ChevronDown className="h-4 w-4" aria-hidden="true" />
        ) : (
          <ChevronRight className="h-4 w-4" aria-hidden="true" />
        )}
        <Terminal className="h-4 w-4 text-neutral-500 dark:text-slate-400" aria-hidden="true" />
        <span>SSH command preview</span>
        <HelpPopover fieldLabel="SSH command preview" testId={`${testIdPrefix}-help`}>
          <p>
            Shows the exact <code className="font-mono text-[11px]">ssh</code> command the dashboard
            will run when you click Launch.
          </p>
          <p className="mt-1">
            Useful for debugging connection issues — copy-paste this into a terminal to reproduce
            manually. The same command is also recorded as the first line of the bot&apos;s log
            after launch.
          </p>
          <p className="mt-1">
            The remote command is wrapped in{" "}
            <code className="font-mono text-[11px]">${"{SHELL:-/bin/bash}"} -lc</code> so the
            operator&apos;s login PATH is loaded (necessary for nvm / homebrew / asdf node
            installs). If <code className="font-mono text-[11px]">npm</code> is still not found on
            the remote, ensure it&apos;s exported by{" "}
            <code className="font-mono text-[11px]">~/.bash_profile</code>,{" "}
            <code className="font-mono text-[11px]">~/.profile</code>, or{" "}
            <code className="font-mono text-[11px]">~/.zprofile</code>.
          </p>
        </HelpPopover>
      </button>
      {subtitle && (
        <p
          className="mt-1 pl-6 text-xs text-neutral-500 dark:text-slate-400"
          data-testid={`${testIdPrefix}-subtitle`}
        >
          {subtitle}
        </p>
      )}
      {open && (
        <div className="mt-3 flex flex-col gap-2">
          {previewQuery.isPending && (
            <p
              className="text-xs text-neutral-500 dark:text-slate-400"
              data-testid={`${testIdPrefix}-loading`}
            >
              Building preview…
            </p>
          )}
          {errorMsg !== null && (
            <p
              className="rounded-md border border-red-200 bg-red-50 px-2 py-1 text-xs text-red-700 dark:border-red-900/40 dark:bg-red-900/20 dark:text-red-300"
              data-testid={`${testIdPrefix}-error`}
              role="alert"
            >
              {errorMsg}
            </p>
          )}
          {display !== "" && errorMsg === null && (
            <>
              <pre
                className="max-h-48 overflow-auto rounded-md bg-neutral-900 px-3 py-2 font-mono text-[11px] leading-relaxed text-slate-100 dark:bg-slate-950"
                data-testid={`${testIdPrefix}-display`}
              >
                {display}
              </pre>
              <div className="flex items-center justify-end">
                <button
                  type="button"
                  onClick={handleCopy}
                  className="inline-flex items-center gap-1 rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:text-slate-200 dark:hover:bg-slate-700"
                  data-testid={`${testIdPrefix}-copy`}
                  aria-label="Copy command to clipboard"
                >
                  {copied ? (
                    <>
                      <Check className="h-3 w-3" aria-hidden="true" />
                      Copied
                    </>
                  ) : (
                    <>
                      <Copy className="h-3 w-3" aria-hidden="true" />
                      Copy
                    </>
                  )}
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
