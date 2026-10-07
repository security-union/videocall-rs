import type { Resolved } from "./run-record";

/** dioxus-ui `context.rs` `DECODE_BUDGET_OVERRIDE_KEY`; locked by `decode-budget.drift.test.ts`. */
export const DECODE_BUDGET_STORAGE_KEY = "vc_decode_budget_override";

/** Parses to `DecodeBudgetOverride::All`, which bypasses the client's adaptive decode-budget loop. */
export const DECODE_BUDGET_OFF_VALUE = "all";

export type DecodeBudget = "client" | "off";

export const DECODE_BUDGET_DEFAULT: DecodeBudget = "client";

export const DECODE_BUDGET_WAIVER_NOTE =
  "Fidelity waiver: probe decode budget off (measures delivery, not client behaviour)";

export function resolveDecodeBudget(raw: string | undefined): Resolved<DecodeBudget> {
  const token = raw?.trim().toLowerCase() ?? "";
  if (token === "" || token === "client") return { kind: "ok", value: "client" };
  if (token === "off") return { kind: "ok", value: "off" };
  return {
    kind: "invalid",
    message: `--decode-budget must be "client" or "off", got "${raw}"`,
  };
}

/** A stored value the client parses as `Auto`, i.e. its own adaptive budget. */
export function isClientDecodeBudget(stored: string | null): boolean {
  return stored === null || stored === "auto";
}

/** `addInitScript` source: on `appOrigin` only, seeds the override before any page script reads it. */
export function buildDecodeBudgetInitScript(appOrigin: string): string {
  return `(() => {
  if (location.origin !== ${JSON.stringify(appOrigin)}) return;
  try {
    localStorage.setItem(${JSON.stringify(DECODE_BUDGET_STORAGE_KEY)}, ${JSON.stringify(DECODE_BUDGET_OFF_VALUE)});
  } catch (e) {
    console.error('[bot] decode-budget override failed to install:', e);
  }
})();`;
}

export class DecodeBudgetNotAppliedError extends Error {
  constructor(readonly observed: string | null) {
    super(
      `--decode-budget off requested but localStorage ${DECODE_BUDGET_STORAGE_KEY} read back ${JSON.stringify(observed)} after join, not "${DECODE_BUDGET_OFF_VALUE}": refusing to run this probe without the waiver it was asked for`,
    );
    this.name = "DecodeBudgetNotAppliedError";
  }
}
