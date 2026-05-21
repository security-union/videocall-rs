import { useQuery } from "@tanstack/react-query";
import { CircleDot } from "lucide-react";

import { api } from "../api/client";
import type { SsoStatusResponse, VpnStatusResponse } from "../api/types";
import { deriveSsoTone, SSO_POLL_INTERVAL_MS, VPN_POLL_INTERVAL_MS } from "./SsoPanel";

/**
 * Persistent header chip surfacing HCL VPN reachability + SSO state
 * freshness at a glance. Mirrors the visual language of the existing
 * "Self-hosted daemon" indicator next to it.
 *
 * Click → opens the {@link SsoPanel} dialog where the operator can
 * trigger a re-capture.
 *
 * Polls VPN every 30s and SSO every 60s — independent intervals (per
 * the spec) so a VPN flap doesn't refetch the SSO file every tick.
 */
export interface SsoChipProps {
  onOpen: () => void;
}

export function SsoChip({ onOpen }: SsoChipProps) {
  const vpnQuery = useQuery({
    queryKey: ["sso", "vpn-status"],
    queryFn: api.vpnStatus,
    refetchInterval: VPN_POLL_INTERVAL_MS,
  });
  const ssoQuery = useQuery({
    queryKey: ["sso", "status"],
    queryFn: api.ssoStatus,
    refetchInterval: SSO_POLL_INTERVAL_MS,
  });

  const vpnUp = vpnQuery.data?.status === "up";
  const ssoTone = deriveSsoTone(ssoQuery.data);

  return (
    <button
      type="button"
      onClick={onOpen}
      className="flex items-center gap-3 rounded-full border border-neutral-200 bg-white px-3 py-1 text-xs hover:bg-neutral-50 dark:border-slate-600 dark:bg-slate-800 dark:hover:bg-slate-700"
      data-testid="sso-chip"
      data-vpn-status={vpnQuery.data?.status ?? "unknown"}
      data-sso-tone={ssoTone}
      aria-label="HCL VPN and SSO status; click to manage"
    >
      <span className="flex items-center gap-1.5 text-neutral-600 dark:text-slate-300">
        <CircleDot
          className={`h-3 w-3 ${
            vpnUp ? "text-emerald-500 dark:text-emerald-400" : "text-red-500 dark:text-red-400"
          }`}
          aria-hidden="true"
        />
        {labelForVpn(vpnQuery.data)}
      </span>
      <span className="text-neutral-300 dark:text-slate-600" aria-hidden="true">
        |
      </span>
      <span className="flex items-center gap-1.5 text-neutral-600 dark:text-slate-300">
        <CircleDot className={`h-3 w-3 ${dotForTone(ssoTone)}`} aria-hidden="true" />
        {labelForSso(ssoQuery.data)}
      </span>
    </button>
  );
}

function labelForVpn(data?: VpnStatusResponse): string {
  if (data === undefined) return "VPN…";
  return data.status === "up" ? "VPN OK" : "VPN unreachable";
}

function labelForSso(data?: SsoStatusResponse): string {
  if (data === undefined) return "SSO…";
  if (!data.exists) return "SSO missing";
  if (data.ageHours === null) return "SSO captured";
  if (data.ageHours > 12) return `SSO stale (${data.ageHours.toFixed(1)}h)`;
  return `SSO ${data.ageHours.toFixed(1)}h ago`;
}

function dotForTone(tone: "green" | "yellow" | "red"): string {
  if (tone === "green") return "text-emerald-500 dark:text-emerald-400";
  if (tone === "yellow") return "text-amber-500 dark:text-amber-400";
  return "text-red-500 dark:text-red-400";
}
