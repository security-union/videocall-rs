import { describe, expect, it, vi } from "vitest";

import { clickHangUp } from "./bot";
import {
  ACTION_BAR_SELECTOR,
  HANG_UP_CANDIDATES,
  PRE_MARKER_UI_BANNER,
  CAMERA_TOOLTIP,
  HANG_UP_SELECTOR,
  MIC_TOOLTIP,
  SCREEN_SHARE_TOOLTIP,
  cameraControlSelector,
  micControlSelector,
  screenShareControlSelector,
} from "./control-buttons";
import { peerListCandidates, screenShareCandidates } from "./control-buttons";
import { newRegistryEntry, type BotRegistryEntry } from "./control/registry";
import { toggleCamera, toggleMicrophone, toggleScreenShare } from "./orchestrator";

// Call-site guards: the builders' drift locks pin them against the Rust source,
// but only these fail when a consumer stops using a builder.
function fakePage(opts: { visible?: (selector: string) => boolean } = {}): {
  page: { locator: ReturnType<typeof vi.fn> };
  seen: string[];
} {
  const seen: string[] = [];
  const visible = opts.visible ?? ((): boolean => true);
  let current = "";
  const btn = {
    first: () => btn,
    isVisible: vi.fn(async () => visible(current)),
    click: vi.fn(async () => undefined),
    hover: vi.fn(async () => undefined),
    waitFor: vi.fn(async () => undefined),
  };
  const page = {
    locator: vi.fn((selector: string) => {
      seen.push(selector);
      current = selector;
      return btn;
    }),
  };
  return { page, seen };
}

function fakeEntry(page: unknown): BotRegistryEntry {
  const e = newRegistryEntry({
    botId: "00000000-0000-4000-8000-000000000000",
    participant: "alice",
    meetingId: "TestRoom",
  } as unknown as Parameters<typeof newRegistryEntry>[0]);
  e.handle = { page } as unknown as BotRegistryEntry["handle"];
  return e;
}

describe("bot.ts leaveMeeting call site", () => {
  it("targets the hang-up testid selector", async () => {
    const { page, seen } = fakePage();
    await clickHangUp(
      page as unknown as Parameters<typeof clickHangUp>[0] & {
        waitForURL: () => Promise<void>;
      },
      () => undefined,
    );
    expect([...new Set(seen)]).toEqual([HANG_UP_SELECTOR]);
  });
});

describe("orchestrator.ts control call sites", () => {
  const controlSelectors = (seen: string[]): string[] => [
    ...new Set(seen.filter((s) => s !== ACTION_BAR_SELECTOR)),
  ];

  it("setMicMuted drives the current-state mic selector", async () => {
    const { page, seen } = fakePage();
    await toggleMicrophone(fakeEntry(page), true);
    expect(controlSelectors(seen)).toEqual([micControlSelector("on")]);
    expect(micControlSelector("on")).toContain(MIC_TOOLTIP.on);
  });

  it("setMicMuted unmute drives the muted-state mic selector", async () => {
    const { page, seen } = fakePage();
    await toggleMicrophone(fakeEntry(page), false);
    expect(controlSelectors(seen)).toEqual([micControlSelector("off")]);
  });

  it("setCameraOff drives the current-state camera selector", async () => {
    const { page, seen } = fakePage();
    await toggleCamera(fakeEntry(page), true);
    expect(controlSelectors(seen)).toEqual([cameraControlSelector("on")]);
    expect(cameraControlSelector("on")).toContain(CAMERA_TOOLTIP.on);
  });

  it("setCameraOff on drives the camera-off-state selector", async () => {
    const { page, seen } = fakePage();
    await toggleCamera(fakeEntry(page), false);
    expect(controlSelectors(seen)).toEqual([cameraControlSelector("off")]);
  });

  it("setScreenShare start drives the idle-state screen-share selector", async () => {
    const { page, seen } = fakePage();
    await toggleScreenShare(fakeEntry(page), true);
    expect(controlSelectors(seen)).toEqual([screenShareControlSelector("off")]);
    expect(screenShareControlSelector("off")).toContain(SCREEN_SHARE_TOOLTIP.off);
  });

  it("setScreenShare stop drives the sharing-state screen-share selector", async () => {
    const { page, seen } = fakePage();
    await toggleScreenShare(fakeEntry(page), false);
    expect(controlSelectors(seen)).toEqual([screenShareControlSelector("on")]);
  });
});

// A target deployed before the #2441 markers must still be drivable, and the
// run output must say so — a silent fallback would misreport the target.
describe("pre-marker UI fallback", () => {
  const preMarker = (candidates: readonly string[]) => (selector: string) =>
    selector !== candidates[0];

  it("clickHangUp falls back to the tooltip arm and reports it", async () => {
    const { page, seen } = fakePage({ visible: preMarker(HANG_UP_CANDIDATES) });
    const warnings: string[] = [];
    await clickHangUp(
      page as unknown as Parameters<typeof clickHangUp>[0],
      () => undefined,
      (m) => warnings.push(m),
    );
    expect(seen).toContain(HANG_UP_CANDIDATES[1]);
    expect(warnings.join("\n")).toContain(PRE_MARKER_UI_BANNER);
  });

  it("setScreenShare falls back to the slot-scoped tooltip arm and reports it", async () => {
    const candidates = screenShareCandidates("off");
    const { page, seen } = fakePage({ visible: preMarker(candidates) });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await toggleScreenShare(fakeEntry(page), true);
    const reported = warn.mock.calls.flat().join("\n");
    warn.mockRestore();
    expect(seen).toContain(candidates[1]);
    expect(reported).toContain(PRE_MARKER_UI_BANNER);
  });

  it("prefers the testid arm when both are present and stays silent", async () => {
    const candidates = screenShareCandidates("off");
    const { page, seen } = fakePage({});
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await toggleScreenShare(fakeEntry(page), true);
    const reported = warn.mock.calls.flat().join("\n");
    warn.mockRestore();
    expect(seen).not.toContain(candidates[1]);
    expect(reported).not.toContain(PRE_MARKER_UI_BANNER);
  });

  it("every fallback arm is scoped — no bare action-bar button selector", () => {
    const arms = [
      ...HANG_UP_CANDIDATES,
      ...screenShareCandidates("on"),
      ...screenShareCandidates("off"),
      ...peerListCandidates("on"),
      ...peerListCandidates("off"),
    ];
    for (const arm of arms) {
      expect(
        arm.startsWith(ACTION_BAR_SELECTOR) || arm.startsWith("[data-testid="),
        `unscoped candidate: ${arm}`,
      ).toBe(true);
    }
  });
});
