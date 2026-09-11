// Turn the per-tile diagnostics indicators ON before the app boots: #2673's ONE "Show
// diagnostics on tiles" checkbox gates the metrics readout, the signal disc and the WS/WT
// transport badge together, OFF by default. MUST precede the first navigation —
// `attendants.rs` reads the key once, at mount (`use_signal(|| load_bool(KEY, false))`) —
// and as an init script it re-seeds on every one, hiding the default from a seeded target.

import type { BrowserContext, Page } from "@playwright/test";

/** Mirrors `MEDIA_METRICS_OVERLAY_KEY` (`dioxus-ui/src/components/media_metrics_overlay.rs`). */
export const MEDIA_METRICS_OVERLAY_KEY = "diagnostics.media_metrics_overlay";

export async function enableDiagnosticsTileIndicators(
  target: BrowserContext | Page,
  enabled: boolean = true,
): Promise<void> {
  // `local_storage::save_bool` stores the bare string "true"/"false", not JSON.
  const value = JSON.stringify(enabled ? "true" : "false");
  const key = JSON.stringify(MEDIA_METRICS_OVERLAY_KEY);
  await target.addInitScript(
    `(() => { try { localStorage.setItem(${key}, ${value}); } catch (_) {} })();`,
  );
}
