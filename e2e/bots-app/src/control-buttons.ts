/** State a control's `aria-label` describes: the CURRENT state, not the action. */
export type ControlState = "on" | "off";

export const ACTION_BAR_SELECTOR = ".video-controls-container";

// Only the live bar wraps its button in `[data-slot]`; the drag-preview clone does not.
export const LIVE_ACTION_BAR_BUTTON = `${ACTION_BAR_SELECTOR} .action-bar-slot-wrapper[data-slot] > button.video-control-button`;

export const MIC_TESTID_SELECTOR = '[data-testid="mic-toggle-button"]';
export const CAMERA_TESTID_SELECTOR = '[data-testid="camera-toggle-button"]';
export const SCREEN_SHARE_TESTID_SELECTOR = '[data-testid="screen-share-button"]';
export const PEER_LIST_TESTID_SELECTOR = '[data-testid="peer-list-button"]';
export const HANG_UP_SELECTOR = '[data-testid="hang-up-button"]';

export const MIC_TOOLTIP: Record<ControlState, string> = {
  on: "Microphone — Mute",
  off: "Microphone — Unmute",
};

// Not MIC_TOOLTIP.off: "Mute" is a substring of "Unmute", but this substring is unambiguous.
const MIC_UNMUTE_ARIA_LABEL = "Unmute";

export const CAMERA_TOOLTIP: Record<ControlState, string> = {
  on: "Stop Video",
  off: "Start Video",
};

export const SCREEN_SHARE_TOOLTIP: Record<ControlState, string> = {
  on: "Screen share — Stop Screen Share",
  off: "Screen share — Share Screen",
};

export const PEER_LIST_TOOLTIP: Record<ControlState, string> = {
  on: "Participants — Close Peers",
  off: "Participants — Open Peers",
};

function liveSlotButton(testidSelector: string, ariaLabel: string): string {
  return `${LIVE_ACTION_BAR_BUTTON}${testidSelector}[aria-label*="${ariaLabel}"]`;
}

export const MIC_UNMUTE_SELECTOR = liveSlotButton(MIC_TESTID_SELECTOR, MIC_UNMUTE_ARIA_LABEL);

export function cameraButtonSelector(ariaLabel: string): string {
  return liveSlotButton(CAMERA_TESTID_SELECTOR, ariaLabel);
}

export function micControlSelector(state: ControlState): string {
  return liveSlotButton(MIC_TESTID_SELECTOR, MIC_TOOLTIP[state]);
}

export function cameraControlSelector(state: ControlState): string {
  return cameraButtonSelector(CAMERA_TOOLTIP[state]);
}

export function screenShareControlSelector(state: ControlState): string {
  return liveSlotButton(SCREEN_SHARE_TESTID_SELECTOR, SCREEN_SHARE_TOOLTIP[state]);
}

export function peerListControlSelector(state: ControlState): string {
  return liveSlotButton(PEER_LIST_TESTID_SELECTOR, PEER_LIST_TOOLTIP[state]);
}

export const PRE_MARKER_UI_BANNER = "PRE_MARKER_UI";

// Pre-#2441 needles. Playwright's `:has-text` is case-insensitive, so casing may differ.
export const HANG_UP_LEGACY_TOOLTIP = "Hang Up";

export const SCREEN_SHARE_LEGACY_TOOLTIP: Record<ControlState, string> = {
  on: "Stop Screen Share",
  off: "Share Screen",
};

export const PEER_LIST_LEGACY_TOOLTIP: Record<ControlState, string> = {
  on: "Close Peers",
  off: "Open Peers",
};

function legacySlotButton(tooltipText: string): string {
  return `${LIVE_ACTION_BAR_BUTTON}:has(.tooltip:has-text("${tooltipText}"))`;
}

// No slot wrapper means the UI predates customize mode, so there is no clone to over-match.
function legacyBarButton(tooltipText: string): string {
  return `${ACTION_BAR_SELECTOR} button.video-control-button:has(.tooltip:has-text("${tooltipText}"))`;
}

export function screenShareCandidates(state: ControlState): readonly string[] {
  const needle = SCREEN_SHARE_LEGACY_TOOLTIP[state];
  return [screenShareControlSelector(state), legacySlotButton(needle), legacyBarButton(needle)];
}

export function peerListCandidates(state: ControlState): readonly string[] {
  const needle = PEER_LIST_LEGACY_TOOLTIP[state];
  return [peerListControlSelector(state), legacySlotButton(needle), legacyBarButton(needle)];
}

// HangUpButton is not an action-bar slot, so it has no slot-scoped arm.
export const HANG_UP_CANDIDATES: readonly string[] = [
  HANG_UP_SELECTOR,
  legacyBarButton(HANG_UP_LEGACY_TOOLTIP),
];

export interface CandidatePage {
  locator(selector: string): {
    isVisible(options?: { timeout?: number }): Promise<boolean>;
  };
}

/**
 * First candidate with a visible match, or null. A non-first match means the
 * target predates the #2441 markers, so its control coverage is degraded.
 */
export async function resolveControlSelector(
  page: CandidatePage,
  candidates: readonly string[],
  label: string,
  onFallback: (message: string) => void,
  timeoutMs = 1_000,
): Promise<string | null> {
  for (let i = 0; i < candidates.length; i++) {
    const selector = candidates[i];
    const visible = await page
      .locator(selector)
      .isVisible({ timeout: timeoutMs })
      .catch(() => false);
    if (!visible) continue;
    if (i > 0) {
      onFallback(
        `${PRE_MARKER_UI_BANNER} ${label}: matched by tooltip text, not data-testid — ` +
          `the target UI predates the #2441 markers`,
      );
    }
    return selector;
  }
  return null;
}
