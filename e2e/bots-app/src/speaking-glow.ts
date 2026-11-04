import type { Resolved } from "./run-record";

/** dioxus-ui `context.rs` `APPEARANCE_GLOW_ENABLED_STORAGE_KEY`; locked by `speaking-glow.drift.test.ts`. */
export const SPEAKING_GLOW_STORAGE_KEY = "vc_appearance_glow_enabled";

/** The only stored value the client loads as `glow_enabled: false`. */
export const SPEAKING_GLOW_OFF_VALUE = "false";

export const SPEAKING_GLOW_ENV = "BOT_SPEAKING_GLOW";

export type SpeakingGlow = "client" | "off";

export const SPEAKING_GLOW_DEFAULT: SpeakingGlow = "client";

export const SPEAKING_GLOW_WAIVER_NOTE =
  "Fidelity waiver: probe speaking glow off (no tile speaking glow and no tile mic-indicator glow, nor the repaints they drive; the participant-list mic keeps its speaking drop-shadow but not the glow colour; received video still paints)";

export function resolveSpeakingGlow(raw: string | undefined): Resolved<SpeakingGlow> {
  const token = raw?.trim().toLowerCase() ?? "";
  if (token === "" || token === "client") return { kind: "ok", value: "client" };
  if (token === "off") return { kind: "ok", value: "off" };
  return {
    kind: "invalid",
    message: `--speaking-glow (or ${SPEAKING_GLOW_ENV}) must be "client" or "off", got "${raw}"`,
  };
}

/** A stored value the client loads as glow enabled, its default. */
export function isClientSpeakingGlow(stored: string | null): boolean {
  return stored !== SPEAKING_GLOW_OFF_VALUE;
}

/** `addInitScript` source: on `appOrigin` only, seeds the preference before any page script reads it. */
export function buildSpeakingGlowInitScript(appOrigin: string): string {
  return `(() => {
  if (location.origin !== ${JSON.stringify(appOrigin)}) return;
  try {
    localStorage.setItem(${JSON.stringify(SPEAKING_GLOW_STORAGE_KEY)}, ${JSON.stringify(SPEAKING_GLOW_OFF_VALUE)});
  } catch (e) {
    console.error('[bot] speaking-glow preference failed to install:', e);
  }
})();`;
}

export type SpeakingGlowVerdict = "ok" | "not-applied" | "inherited-off";

/** What a bot does with the value read back after join: run, fail the launch, or warn. */
export function speakingGlowVerdict(
  requested: SpeakingGlow,
  readback: string | null,
): SpeakingGlowVerdict {
  if (requested === "off") return readback === SPEAKING_GLOW_OFF_VALUE ? "ok" : "not-applied";
  return isClientSpeakingGlow(readback) ? "ok" : "inherited-off";
}

export class SpeakingGlowNotAppliedError extends Error {
  constructor(readonly observed: string | null) {
    super(
      `--speaking-glow off requested but localStorage ${SPEAKING_GLOW_STORAGE_KEY} read back ${JSON.stringify(observed)} after join, not "${SPEAKING_GLOW_OFF_VALUE}": refusing to run this probe without the waiver it was asked for`,
    );
    this.name = "SpeakingGlowNotAppliedError";
  }
}
