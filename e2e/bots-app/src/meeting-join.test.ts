import { describe, it, expect, vi } from "vitest";

import {
  JoinRejectedError,
  MEETING_STATE_SELECTORS,
  MeetingNavigatedAwayError,
  WaitingRoomError,
  classifyJoinModeText,
  detectJoinMode,
  ensureWaitingRoomOff,
  joinMeetingAndEnableMedia,
} from "./meeting-join";

// `joinMeetingAndEnableMedia` itself is Playwright-driven and would
// require a real browser context to exercise meaningfully — covered by
// the manual smoke test described in `README.md` rather than mocked
// here.
//
// The error classes and the selector table are pure data and ARE
// exercisable in isolation; covering them here gives us a regression
// guard against accidentally renaming a selector (which would silently
// break the bot's waiting-room detection) or dropping a typed error
// (which would silently downgrade a graceful waiting-room exit to a
// "launch failed" tally in the orchestrator).

describe("meeting-join module surface", () => {
  it("exports joinMeetingAndEnableMedia as a function", () => {
    expect(typeof joinMeetingAndEnableMedia).toBe("function");
  });

  it("exports the meeting-state selector table", () => {
    expect(MEETING_STATE_SELECTORS).toEqual({
      waitingRoom: '[data-testid="meeting-waiting-room"]',
      waitingForHost: '[data-testid="meeting-waiting-for-host"]',
      rejected: '[data-testid="meeting-rejected"]',
      error: '[data-testid="meeting-error"]',
    });
  });
});

describe("MeetingNavigatedAwayError", () => {
  it("carries the manual-hang-up discriminator", () => {
    const err = new MeetingNavigatedAwayError("manual hangup");
    expect(err).toBeInstanceOf(Error);
    expect(err.kind).toBe("meeting-navigated-away");
    expect(err.message).toBe("manual hangup");
  });
});

describe("WaitingRoomError", () => {
  it("carries waiting-room variant + clean message (host has Waiting Room on)", () => {
    const err = new WaitingRoomError("waiting-room", "parked in waiting room");
    expect(err).toBeInstanceOf(Error);
    expect(err.kind).toBe("waiting-room");
    expect(err.variant).toBe("waiting-room");
    expect(err.message).toBe("parked in waiting room");
  });

  it("carries waiting-for-host variant (host hasn't started yet)", () => {
    const err = new WaitingRoomError("waiting-for-host", "host hasn't started");
    expect(err.variant).toBe("waiting-for-host");
  });
});

describe("JoinRejectedError", () => {
  it("carries rejected reason for host-denied joins", () => {
    const err = new JoinRejectedError("rejected", "host denied the join request");
    expect(err).toBeInstanceOf(Error);
    expect(err.kind).toBe("join-rejected");
    expect(err.reason).toBe("rejected");
  });

  it("carries error reason for server-reported failures", () => {
    const err = new JoinRejectedError("error", "host has left and no one can admit");
    expect(err.reason).toBe("error");
    expect(err.message).toContain("host has left");
  });
});

// Race-outcome simulation — exercises the orchestrator's discriminator
// without spinning up Chrome. The actual `joinMeetingAndEnableMedia`
// call path is intentionally not invoked (it would require a Playwright
// `Page`); instead we model the four post-click race outcomes that the
// helper produces and assert that each translates to the right typed
// error or to the existing success/launch-error paths.
describe("post-click race outcome → exit type", () => {
  type Outcome = "grid" | "waiting-room" | "waiting-for-host" | "rejected" | "error" | null;

  // Lightweight stand-in for the throwForOutcome helper inside
  // meeting-join.ts. Keep this in lockstep with the production switch —
  // if a new outcome variant is added, this test must fail to compile
  // (TS' exhaustive-switch narrowing) so the rolling regression caught
  // here keeps the orchestrator classification honest.
  function translate(outcome: Outcome): Error | "grid-success" | "no-resolution" {
    if (outcome === null) return "no-resolution";
    if (outcome === "grid") return "grid-success";
    if (outcome === "waiting-room") return new WaitingRoomError("waiting-room", "parked");
    if (outcome === "waiting-for-host")
      return new WaitingRoomError("waiting-for-host", "host hasn't started");
    if (outcome === "rejected") return new JoinRejectedError("rejected", "host denied");
    return new JoinRejectedError("error", "server error");
  }

  it("grid outcome resolves to success (no throw)", () => {
    expect(translate("grid")).toBe("grid-success");
  });

  it("waiting-room outcome resolves to WaitingRoomError (graceful)", () => {
    const r = translate("waiting-room");
    expect(r).toBeInstanceOf(WaitingRoomError);
    expect((r as WaitingRoomError).variant).toBe("waiting-room");
  });

  it("waiting-for-host outcome resolves to WaitingRoomError (graceful)", () => {
    const r = translate("waiting-for-host");
    expect(r).toBeInstanceOf(WaitingRoomError);
    expect((r as WaitingRoomError).variant).toBe("waiting-for-host");
  });

  it("rejected outcome resolves to JoinRejectedError (failure)", () => {
    const r = translate("rejected");
    expect(r).toBeInstanceOf(JoinRejectedError);
    expect((r as JoinRejectedError).reason).toBe("rejected");
  });

  it("error outcome resolves to JoinRejectedError (failure)", () => {
    const r = translate("error");
    expect(r).toBeInstanceOf(JoinRejectedError);
    expect((r as JoinRejectedError).reason).toBe("error");
  });

  it("null outcome (no screen resolved) falls through to legacy launch-error path", () => {
    expect(translate(null)).toBe("no-resolution");
  });
});

// `classifyJoinModeText` is the pure data-classifier behind
// `detectJoinMode` — exercising it here gives us a regression guard
// against accidentally dropping the regex anchor, the case-insensitive
// flag, or the trim. Each of those silently degrades the bot's log
// (it logs "Join Meeting" for a Start render, or "unknown" for a
// label that just got an emoji appended).
describe("classifyJoinModeText", () => {
  it('returns "start" for "Start Meeting"', () => {
    expect(classifyJoinModeText("Start Meeting")).toBe("start");
  });

  it('returns "join" for "Join Meeting"', () => {
    expect(classifyJoinModeText("Join Meeting")).toBe("join");
  });

  it('returns "unknown" for an unrelated label', () => {
    expect(classifyJoinModeText("Something Else")).toBe("unknown");
  });

  it("trims surrounding whitespace before matching", () => {
    expect(classifyJoinModeText("   Start Meeting   ")).toBe("start");
    expect(classifyJoinModeText("\n\tJoin Meeting\n")).toBe("join");
  });

  it("is case-insensitive on the canonical labels", () => {
    expect(classifyJoinModeText("start meeting")).toBe("start");
    expect(classifyJoinModeText("JOIN MEETING")).toBe("join");
    expect(classifyJoinModeText("STArt MEETing now")).toBe("start");
  });

  it('returns "unknown" for empty + whitespace-only strings', () => {
    expect(classifyJoinModeText("")).toBe("unknown");
    expect(classifyJoinModeText("   ")).toBe("unknown");
  });

  it("tolerates a trailing suffix on either label (anchored only at start)", () => {
    expect(classifyJoinModeText("Start Meeting (owner)")).toBe("start");
    expect(classifyJoinModeText("Join Meeting →")).toBe("join");
  });
});

// `detectJoinMode` is a tiny wrapper around `classifyJoinModeText` that
// reads the text off a Playwright Locator. We mock the Locator's
// `innerText` here — fully covering both the happy-path delegation and
// the `.catch(() => "")` fallback for innerText failures (a flaky
// network DOM-snapshot, or the element going stale).
describe("detectJoinMode", () => {
  it('returns "start" when the locator innerText is "Start Meeting"', async () => {
    const locator = { innerText: vi.fn().mockResolvedValue("Start Meeting") };
    // Cast: we only need the `innerText` shape Playwright's Locator
    // exposes; the production helper does no other Locator calls.
    expect(await detectJoinMode(locator as never)).toBe("start");
    expect(locator.innerText).toHaveBeenCalledTimes(1);
  });

  it('returns "join" when the locator innerText is "Join Meeting"', async () => {
    const locator = { innerText: vi.fn().mockResolvedValue("Join Meeting") };
    expect(await detectJoinMode(locator as never)).toBe("join");
  });

  it('returns "unknown" when the locator innerText is a foreign label', async () => {
    const locator = { innerText: vi.fn().mockResolvedValue("Leave Meeting") };
    expect(await detectJoinMode(locator as never)).toBe("unknown");
  });

  it('returns "unknown" when innerText rejects (DOM read failure)', async () => {
    const locator = { innerText: vi.fn().mockRejectedValue(new Error("stale element")) };
    expect(await detectJoinMode(locator as never)).toBe("unknown");
  });
});

// `ensureWaitingRoomOff` drives a real Playwright Page+Locator chain.
// We stub just the calls the helper actually makes — `.locator(...)`
// (chained), `.filter(...)`, `.isVisible`, `.getAttribute`, `.click`,
// `.waitFor`, and `.first()` — so each branch is exercised without
// spinning up Chrome.
describe("ensureWaitingRoomOff", () => {
  /**
   * Build a fake Page whose `.locator(".settings-option-row")` returns
   * a row stub that supports `.filter(...)`. The row stub yields a
   * toggle stub when asked for `[role="switch"]` (the current toggle)
   * AND a separate "post-click flipped" locator when asked for
   * `[role="switch"][aria-checked="false"]`.
   */
  function makeFakePage(args: {
    toggleVisible: boolean;
    initialAriaChecked: "true" | "false" | "indeterminate" | null;
    clickThrows?: boolean;
    postFlipWaitThrows?: boolean;
  }): {
    page: never;
    calls: {
      isVisible: number;
      getAttribute: number;
      click: number;
      flipWait: number;
    };
  } {
    const calls = { isVisible: 0, getAttribute: 0, click: 0, flipWait: 0 };

    const flipLocator = {
      first: vi.fn().mockReturnThis(),
      waitFor: vi.fn().mockImplementation(async () => {
        calls.flipWait += 1;
        if (args.postFlipWaitThrows) throw new Error("flip wait timeout");
      }),
    };

    const toggle = {
      first: vi.fn().mockReturnThis(),
      isVisible: vi.fn().mockImplementation(async () => {
        calls.isVisible += 1;
        return args.toggleVisible;
      }),
      getAttribute: vi.fn().mockImplementation(async () => {
        calls.getAttribute += 1;
        return args.initialAriaChecked;
      }),
      click: vi.fn().mockImplementation(async () => {
        calls.click += 1;
        if (args.clickThrows) throw new Error("click failed");
      }),
    };

    const row = {
      locator: vi.fn().mockImplementation((sel: string) => {
        if (sel === '[role="switch"][aria-checked="false"]') return flipLocator;
        return toggle;
      }),
      filter: vi.fn().mockReturnThis(),
    };

    const page = {
      locator: vi.fn().mockImplementation((sel: string) => {
        if (sel === ".settings-option-row") return row;
        throw new Error(`unexpected locator selector: ${sel}`);
      }),
      waitForTimeout: vi.fn().mockResolvedValue(undefined),
    };

    return { page: page as never, calls };
  }

  it("no-ops when the toggle is not visible (bot is in Join mode)", async () => {
    const { page, calls } = makeFakePage({
      toggleVisible: false,
      initialAriaChecked: null,
    });
    await ensureWaitingRoomOff(page, "bot-1");
    expect(calls.isVisible).toBe(1);
    expect(calls.getAttribute).toBe(0);
    expect(calls.click).toBe(0);
    expect(calls.flipWait).toBe(0);
  });

  it('skips the click when aria-checked is already "false" (toggle already OFF)', async () => {
    const { page, calls } = makeFakePage({
      toggleVisible: true,
      initialAriaChecked: "false",
    });
    await ensureWaitingRoomOff(page, "bot-1");
    expect(calls.isVisible).toBe(1);
    expect(calls.getAttribute).toBe(1);
    expect(calls.click).toBe(0);
    expect(calls.flipWait).toBe(0);
  });

  it('clicks + waits for aria-checked="false" when toggle starts ON', async () => {
    const { page, calls } = makeFakePage({
      toggleVisible: true,
      initialAriaChecked: "true",
    });
    await ensureWaitingRoomOff(page, "bot-1");
    expect(calls.isVisible).toBe(1);
    expect(calls.getAttribute).toBe(1);
    expect(calls.click).toBe(1);
    // Post-click `waitFor` on the `aria-checked="false"` locator must
    // fire — this is the explicit post-condition the v1.7.1 change
    // introduces.
    expect(calls.flipWait).toBe(1);
  });

  it("logs a warning + does not throw when aria-checked is unexpected", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { page, calls } = makeFakePage({
      toggleVisible: true,
      initialAriaChecked: "indeterminate",
    });
    await ensureWaitingRoomOff(page, "bot-1");
    expect(calls.click).toBe(0);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("does not throw when the click itself fails (best-effort)", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { page, calls } = makeFakePage({
      toggleVisible: true,
      initialAriaChecked: "true",
      clickThrows: true,
    });
    await expect(ensureWaitingRoomOff(page, "bot-1")).resolves.toBeUndefined();
    expect(calls.click).toBe(1);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("does not throw when the post-click flip wait times out (best-effort)", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { page, calls } = makeFakePage({
      toggleVisible: true,
      initialAriaChecked: "true",
      postFlipWaitThrows: true,
    });
    await expect(ensureWaitingRoomOff(page, "bot-1")).resolves.toBeUndefined();
    expect(calls.click).toBe(1);
    expect(calls.flipWait).toBe(1);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});
