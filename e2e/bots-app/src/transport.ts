/** The run-manifest's `transport_intended` values (`scripts/quality/cq_manifest.py` `TRANSPORTS`). */
export type TransportIntent = "auto" | "websocket" | "webtransport";
export type PinnedTransport = Exclude<TransportIntent, "auto">;

export const TRANSPORT_ENV = "BOT_TRANSPORT";
export const TRANSPORT_WS_EVERY_ENV = "BOT_TRANSPORT_WS_EVERY";

/** dioxus-ui `RuntimeConfig.default_transport`; the value must be a `TransportPreference` Display arm. */
export const DEFAULT_TRANSPORT_CONFIG_KEY = "defaultTransport";

/** dioxus-ui `context.rs` `TRANSPORT_{STICKY,PREF}_KEY` (localStorage) and `TRANSPORT_SESSION_KEY` (sessionStorage). */
export const TRANSPORT_STICKY_KEY = "vc_transport_sticky";
export const TRANSPORT_PREF_KEY = "vc_transport_preference";
export const TRANSPORT_SESSION_KEY = "vc_transport_session";

export type TransportResult =
  | { kind: "ok"; value: TransportIntent }
  | { kind: "invalid"; message: string };

const TOKENS: Record<string, TransportIntent> = {
  auto: "auto",
  ws: "websocket",
  websocket: "websocket",
  wt: "webtransport",
  webtransport: "webtransport",
};

/** `--transport` / `BOT_TRANSPORT`: unset or empty is `auto`. */
export function resolveTransport(raw: string | undefined): TransportResult {
  const token = raw?.trim().toLowerCase() ?? "";
  if (token === "") return { kind: "ok", value: "auto" };
  const value = TOKENS[token];
  if (value !== undefined) return { kind: "ok", value };
  return {
    kind: "invalid",
    message: `--transport (or ${TRANSPORT_ENV}) must be ws, wt or auto, got "${raw}"`,
  };
}

export function isPinned(t: TransportIntent | null | undefined): t is PinnedTransport {
  return t === "websocket" || t === "webtransport";
}

/** `addInitScript` source: on `appOrigin` only, drops any remembered choice before the client reads it. */
export function buildTransportInitScript(appOrigin: string): string {
  return `(() => {
  if (location.origin !== ${JSON.stringify(appOrigin)}) return;
  try {
    localStorage.removeItem(${JSON.stringify(TRANSPORT_STICKY_KEY)});
    localStorage.removeItem(${JSON.stringify(TRANSPORT_PREF_KEY)});
    sessionStorage.removeItem(${JSON.stringify(TRANSPORT_SESSION_KEY)});
  } catch (e) {
    console.error('[bot] transport preference reset failed:', e);
  }
})();`;
}

export interface TransportApplied {
  preference: string;
  source: string;
  wt_urls: number;
  ws_urls: number;
}

export type ActiveTransport = PinnedTransport | "none";

export type TransportEvent =
  | { kind: "applied"; applied: TransportApplied }
  | { kind: "elected"; active: ActiveTransport };

const APPLIED_LINE =
  /Transport preference applied: pref=(\S+) source=(\S+) wt_urls=(\d+) ws_urls=(\d+)/;

/** videocall-client `format_election_decision`; `active` is `<ws|wt>_<i>[_g<N>]` or `none`. */
const ELECTION_LINE = /Election decision: .*?\bactive=(\S+)/;

export function parseTransportLine(text: string): TransportEvent | null {
  const a = APPLIED_LINE.exec(text);
  if (a !== null) {
    return {
      kind: "applied",
      applied: { preference: a[1], source: a[2], wt_urls: Number(a[3]), ws_urls: Number(a[4]) },
    };
  }
  const e = ELECTION_LINE.exec(text);
  if (e === null) return null;
  const id = e[1];
  if (id === "none") return { kind: "elected", active: "none" };
  if (id.startsWith("ws_")) return { kind: "elected", active: "websocket" };
  if (id.startsWith("wt_")) return { kind: "elected", active: "webtransport" };
  return null;
}

/** Why a pinned bot is not running the transport it was given, or `null` when it is. */
export function transportMismatch(
  intended: PinnedTransport,
  applied: TransportApplied,
): string | null {
  if (applied.source !== "default") {
    return `a remembered choice (source=${applied.source}) set pref=${applied.preference}, so ${DEFAULT_TRANSPORT_CONFIG_KEY} was not consulted`;
  }
  if (applied.preference !== intended) {
    return `the client applied pref=${applied.preference} from its default, not ${intended}: the ${DEFAULT_TRANSPORT_CONFIG_KEY} override did not land or the deployed client predates it`;
  }
  return null;
}

export function isFallback(
  previous: PinnedTransport | null,
  next: ActiveTransport,
  applied: TransportApplied | null,
): boolean {
  return (
    applied !== null &&
    previous !== null &&
    next !== "none" &&
    next !== previous &&
    applied.preference === previous &&
    applied.wt_urls > 0 &&
    applied.ws_urls > 0
  );
}

export class TransportNotAppliedError extends Error {
  constructor(
    readonly intended: PinnedTransport,
    readonly reason: string,
  ) {
    super(`--transport ${intended} requested but ${reason}: refusing to run this bot`);
    this.name = "TransportNotAppliedError";
  }
}
