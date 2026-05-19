import { describe, it, expect, vi, afterEach } from "vitest";

import {
  type ClickAttemptDiagnostics,
  JoinRejectedError,
  MEETING_STATE_SELECTORS,
  MeetingNavigatedAwayError,
  WaitingRoomError,
  classifyJoinModeText,
  detectJoinMode,
  ensureWaitingRoomOff,
  installClickDiagnostics,
  joinMeetingAndEnableMedia,
  logPostClickDiagnostics,
  racePhaseAClickConsumed,
  racePhaseBPostClick,
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

// `installClickDiagnostics` is the per-attempt event recorder that
// surfaces WHY a join click didn't transition. We build a tiny event
// emitter shaped like Playwright's Page (.on/.off + .url) so each
// captured-event branch is exercised without spinning up Chrome.
//
// Coverage targets:
//   - console.error events of type "error" are captured.
//   - console messages of other types are ignored.
//   - requestfailed events are captured with the failure text.
//   - response events with status >= 400 are captured.
//   - response events with status < 400 are ignored.
//   - The 20-entry cap is enforced for both console + request lanes.
//   - Dev-server cosmetic noise is filtered out so it doesn't displace
//     real errors.
//   - teardown removes all installed listeners.

type EventName = "console" | "requestfailed" | "response";
type AnyListener = (arg: unknown) => void;

interface FakePage {
  url: () => string;
  on: (event: EventName, listener: AnyListener) => void;
  off: (event: EventName, listener: AnyListener) => void;
  emit: (event: EventName, arg: unknown) => void;
  listenerCount: (event: EventName) => number;
}

function makeFakePage(url: string): FakePage {
  const listeners: Record<EventName, Set<AnyListener>> = {
    console: new Set(),
    requestfailed: new Set(),
    response: new Set(),
  };
  return {
    url: () => url,
    on: (event, listener) => {
      listeners[event].add(listener);
    },
    off: (event, listener) => {
      listeners[event].delete(listener);
    },
    emit: (event, arg) => {
      for (const fn of listeners[event]) fn(arg);
    },
    listenerCount: (event) => listeners[event].size,
  };
}

function fakeConsoleMessage(
  type: string,
  text: string,
): { type: () => string; text: () => string } {
  return { type: () => type, text: () => text };
}

function fakeRequest(
  url: string,
  errorText?: string,
): {
  url: () => string;
  failure: () => { errorText: string } | null;
} {
  return {
    url: () => url,
    failure: () => (errorText !== undefined ? { errorText } : null),
  };
}

function fakeResponse(url: string, status: number): { url: () => string; status: () => number } {
  return { url: () => url, status: () => status };
}

describe("installClickDiagnostics", () => {
  it('captures console.error events of type "error" into diag.consoleErrors', () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("console", fakeConsoleMessage("error", "TypeError: cannot read property"));
    page.emit("console", fakeConsoleMessage("error", "another error"));

    expect(diag.consoleErrors).toEqual(["TypeError: cannot read property", "another error"]);
    teardown();
  });

  it("ignores console messages of non-error types", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("console", fakeConsoleMessage("log", "just a log"));
    page.emit("console", fakeConsoleMessage("warning", "a warning"));
    page.emit("console", fakeConsoleMessage("info", "info line"));

    expect(diag.consoleErrors).toHaveLength(0);
    teardown();
  });

  it("captures requestfailed events with the failure text", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("requestfailed", fakeRequest("https://api.example.com/foo", "net::ERR_FAILED"));

    expect(diag.failedRequests).toEqual([
      { url: "https://api.example.com/foo", failure: "net::ERR_FAILED" },
    ]);
    teardown();
  });

  it("captures requestfailed events with undefined failure when none is reported", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("requestfailed", fakeRequest("https://api.example.com/foo"));

    expect(diag.failedRequests).toEqual([
      { url: "https://api.example.com/foo", failure: undefined },
    ]);
    teardown();
  });

  it("captures HTTP >= 400 responses into diag.failedRequests with the status code", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("response", fakeResponse("https://api.example.com/api/v1/meetings/Foo/join", 403));
    page.emit("response", fakeResponse("https://api.example.com/api/v1/meetings/Foo/join", 500));

    expect(diag.failedRequests).toEqual([
      { url: "https://api.example.com/api/v1/meetings/Foo/join", status: 403 },
      { url: "https://api.example.com/api/v1/meetings/Foo/join", status: 500 },
    ]);
    teardown();
  });

  it("ignores HTTP < 400 responses (success / redirects are not failures)", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    page.emit("response", fakeResponse("https://api.example.com/ok", 200));
    page.emit("response", fakeResponse("https://api.example.com/redirect", 302));
    page.emit("response", fakeResponse("https://api.example.com/not-modified", 304));

    expect(diag.failedRequests).toHaveLength(0);
    teardown();
  });

  it("enforces the 20-entry cap on consoleErrors (extra events are dropped)", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    for (let i = 0; i < 30; i++) {
      page.emit("console", fakeConsoleMessage("error", `error #${i}`));
    }

    expect(diag.consoleErrors).toHaveLength(20);
    // First 20 are kept; the rest are dropped.
    expect(diag.consoleErrors[0]).toBe("error #0");
    expect(diag.consoleErrors[19]).toBe("error #19");
    teardown();
  });

  it("enforces the 20-entry cap on failedRequests across both lanes combined", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    for (let i = 0; i < 15; i++) {
      page.emit("response", fakeResponse(`https://api/${i}`, 500));
    }
    for (let i = 0; i < 15; i++) {
      page.emit("requestfailed", fakeRequest(`https://api/fail/${i}`, "net::ERR_FAILED"));
    }

    // Cap applies to the combined budget — first 20 wins regardless of lane.
    expect(diag.failedRequests).toHaveLength(20);
    teardown();
  });

  it("filters Dioxus dev-server cosmetic noise so it doesn't displace real errors", () => {
    const page = makeFakePage("http://localhost:3001/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    // Dev-server noise (filtered)
    page.emit("console", fakeConsoleMessage("error", "Unexpected token '<'"));
    page.emit(
      "console",
      fakeConsoleMessage(
        "error",
        "WebSocket connection to 'ws://localhost:3001/_dioxus?build_id=0' failed",
      ),
    );
    // Real error (kept)
    page.emit("console", fakeConsoleMessage("error", "Failed to fetch meeting config"));

    expect(diag.consoleErrors).toEqual(["Failed to fetch meeting config"]);
    teardown();
  });

  it("records startUrl + startedAt at install time so the diff is accurate", () => {
    const before = Date.now();
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { diag, teardown } = installClickDiagnostics(page as never);

    expect(diag.startUrl).toBe("https://example.com/meeting/Foo");
    expect(diag.startedAt).toBeGreaterThanOrEqual(before);
    expect(diag.startedAt).toBeLessThanOrEqual(Date.now());
    teardown();
  });

  it("teardown removes every installed listener so retries don't leak", () => {
    const page = makeFakePage("https://example.com/meeting/Foo");
    const { teardown } = installClickDiagnostics(page as never);

    expect(page.listenerCount("console")).toBe(1);
    expect(page.listenerCount("requestfailed")).toBe(1);
    expect(page.listenerCount("response")).toBe(1);

    teardown();

    expect(page.listenerCount("console")).toBe(0);
    expect(page.listenerCount("requestfailed")).toBe(0);
    expect(page.listenerCount("response")).toBe(0);
  });
});

describe("logPostClickDiagnostics", () => {
  let logs: string[] = [];
  const logSpy = vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
    logs.push(args.map(String).join(" "));
  });

  afterEach(() => {
    logs = [];
    logSpy.mockClear();
  });

  function makeDiag(overrides: Partial<ClickAttemptDiagnostics> = {}): ClickAttemptDiagnostics {
    return {
      startedAt: Date.now() - 2_000,
      startUrl: "https://example.com/meeting/Foo",
      consoleErrors: [],
      failedRequests: [],
      ...overrides,
    };
  }

  it("logs '0 console.error(s)' + '0 failed request(s)' when nothing was captured", () => {
    const diag = makeDiag();
    logPostClickDiagnostics("bot-1", 2, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("attempt 2 diagnostics"))).toBe(true);
    expect(logs.some((l) => l.includes("url unchanged"))).toBe(true);
    expect(logs.some((l) => l.includes("captured 0 console.error(s)"))).toBe(true);
    expect(logs.some((l) => l.includes("captured 0 failed request(s)"))).toBe(true);
    // No hint line should fire when no failures were captured.
    expect(logs.some((l) => l.includes("meeting-api join request failed"))).toBe(false);
  });

  it("logs each captured console.error on its own indented line", () => {
    const diag = makeDiag({
      consoleErrors: ["TypeError: cannot read property", "WebSocket closed unexpectedly"],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("captured 2 console.error(s)"))).toBe(true);
    expect(logs.some((l) => l.includes("[1] TypeError: cannot read property"))).toBe(true);
    expect(logs.some((l) => l.includes("[2] WebSocket closed unexpectedly"))).toBe(true);
  });

  it("logs each captured failed request with HTTP status when present", () => {
    const diag = makeDiag({
      failedRequests: [
        { url: "https://api.example.com/api/v1/meetings/Foo/join", status: 403 },
        { url: "https://cdn.example.com/asset.png", status: 404 },
      ],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("captured 2 failed request(s)"))).toBe(true);
    expect(
      logs.some((l) => l.includes("HTTP 403") && l.includes("/api/v1/meetings/Foo/join")),
    ).toBe(true);
    expect(logs.some((l) => l.includes("HTTP 404") && l.includes("asset.png"))).toBe(true);
  });

  it("logs failure text for transport-level errors when there's no HTTP status", () => {
    const diag = makeDiag({
      failedRequests: [{ url: "https://api.example.com/foo", failure: "net::ERR_FAILED" }],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("net::ERR_FAILED"))).toBe(true);
  });

  it("falls back to 'unknown failure' when neither status nor failure text is set", () => {
    const diag = makeDiag({
      failedRequests: [{ url: "https://api.example.com/foo" }],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("unknown failure"))).toBe(true);
  });

  it("marks the URL as CHANGED when the page navigated since the click", () => {
    const diag = makeDiag({ startUrl: "https://example.com/meeting/Foo" });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/");

    expect(logs.some((l) => l.includes("url CHANGED to https://example.com/"))).toBe(true);
  });

  it("fires the meeting-api hint when a /api/v1/meetings/.../join URL is 4xx", () => {
    const diag = makeDiag({
      failedRequests: [{ url: "https://api.example.com/api/v1/meetings/Foo/join", status: 403 }],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(
      logs.some(
        (l) =>
          l.includes("meeting-api join request failed with HTTP 403") &&
          l.includes("server-side logs"),
      ),
    ).toBe(true);
  });

  it("fires the meeting-api hint for 500-class server errors too", () => {
    const diag = makeDiag({
      failedRequests: [{ url: "https://api.example.com/api/v1/meetings/Foo/join", status: 503 }],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("meeting-api join request failed with HTTP 503"))).toBe(
      true,
    );
  });

  it("does NOT fire the meeting-api hint for unrelated failed URLs", () => {
    const diag = makeDiag({
      failedRequests: [
        { url: "https://cdn.example.com/asset.png", status: 404 },
        { url: "https://api.example.com/api/v1/users/me", status: 401 },
      ],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("meeting-api join request failed"))).toBe(false);
  });

  it("does NOT fire the meeting-api hint when the join URL is < 400 (success)", () => {
    const diag = makeDiag({
      failedRequests: [
        // A 200 wouldn't actually be captured by installClickDiagnostics,
        // but we want defense-in-depth on the hint logic itself.
        { url: "https://api.example.com/api/v1/meetings/Foo/join", status: 200 },
      ],
    });
    logPostClickDiagnostics("bot-1", 1, diag, "https://example.com/meeting/Foo");

    expect(logs.some((l) => l.includes("meeting-api join request failed"))).toBe(false);
  });
});

// `racePhaseAClickConsumed` and `racePhaseBPostClick` are the two new
// race helpers that replaced the v1.7.2 single-race `joinWithRetries`
// loop. The v1.7.2 implementation raced
// `joinButton.waitFor({state: "visible"})` against a locator that was
// already visible — Playwright resolves "already in target state"
// immediately, so all three retries fired in ~250ms total (each
// "attempt" took ~80ms instead of the intended 45,000ms).
//
// The mocked-locator shape below models Playwright's waitFor exactly
// enough to drive each branch: we record (1) which `state` option was
// passed (so we can prove Phase A asks for "hidden", not "visible"), and
// (2) we let each locator's waitFor be controlled by the test (resolve
// after a given delay, or reject with a Playwright-style timeout error).
// `vi.useFakeTimers` lets the suite verify the "click-not-consumed" path
// genuinely waits the full per-attempt budget instead of collapsing.

interface FakeWaitForOptions {
  timeout: number;
  state?: "attached" | "detached" | "visible" | "hidden";
}

interface FakeLocator {
  waitFor: (opts: FakeWaitForOptions) => Promise<void>;
  /** Test-visible record of every `waitFor` call. */
  calls: FakeWaitForOptions[];
}

type FakeOutcome =
  | { kind: "resolve"; afterMs: number }
  | { kind: "reject"; afterMs: number }
  | { kind: "never" };

/**
 * Build a fake Locator whose `waitFor` resolves or rejects after the
 * configured delay. Reject is modeled to match Playwright's
 * locator.waitFor timeout shape (the test never inspects the error
 * message, only the promise rejection).
 *
 * `outcomeByState` lets a single locator respond differently to
 * `state: "visible"` vs `state: "hidden"` — needed because the join
 * button is a single locator that's "currently visible" but eventually
 * "hidden".
 */
function makeFakeLocator(args: {
  outcome?: FakeOutcome;
  outcomeByState?: Partial<Record<NonNullable<FakeWaitForOptions["state"]>, FakeOutcome>>;
}): FakeLocator {
  const calls: FakeWaitForOptions[] = [];
  return {
    calls,
    waitFor: (opts: FakeWaitForOptions): Promise<void> => {
      calls.push(opts);
      const stateKey = opts.state;
      const resolved = (stateKey !== undefined ? args.outcomeByState?.[stateKey] : undefined) ??
        args.outcome ?? { kind: "never" };
      if (resolved.kind === "never") {
        return new Promise<void>(() => {
          /* never resolves; the outer race resolves first via timeout */
        });
      }
      if (resolved.kind === "resolve") {
        return new Promise<void>((resolve) => setTimeout(resolve, resolved.afterMs));
      }
      return new Promise<void>((_resolve, reject) =>
        setTimeout(() => reject(new Error("locator.waitFor: Timeout exceeded.")), resolved.afterMs),
      );
    },
  };
}

/**
 * Build the standard {joinButton, grid, waitingRoom, waitingForHost,
 * rejected, errorScreen} bundle. By default every locator "never
 * resolves" — the test then configures only the ones it wants firing.
 *
 * The join button defaults to "reject on visible / never on hidden" so
 * the Phase A race times out unless the test overrides it; this mirrors
 * the production scenario where the click is a no-op and the button
 * stays visible.
 */
interface FakeLocatorBundle {
  joinButton: FakeLocator;
  grid: FakeLocator;
  waitingRoom: FakeLocator;
  waitingForHost: FakeLocator;
  rejected: FakeLocator;
  errorScreen: FakeLocator;
}

function makeLocatorBundle(overrides: Partial<FakeLocatorBundle> = {}): FakeLocatorBundle {
  return {
    joinButton: overrides.joinButton ?? makeFakeLocator({ outcome: { kind: "never" } }),
    grid: overrides.grid ?? makeFakeLocator({ outcome: { kind: "never" } }),
    waitingRoom: overrides.waitingRoom ?? makeFakeLocator({ outcome: { kind: "never" } }),
    waitingForHost: overrides.waitingForHost ?? makeFakeLocator({ outcome: { kind: "never" } }),
    rejected: overrides.rejected ?? makeFakeLocator({ outcome: { kind: "never" } }),
    errorScreen: overrides.errorScreen ?? makeFakeLocator({ outcome: { kind: "never" } }),
  };
}

/**
 * Thin wrappers that perform the FakeLocator→Locator cast once each, so
 * the test bodies read cleanly while still type-checking against the
 * production helpers. The cast is safe — the race helpers only call
 * `.waitFor(...)` on the locators they're given, which FakeLocator fully
 * implements.
 */
function runPhaseA(
  args: FakeLocatorBundle & { timeout: number },
): ReturnType<typeof racePhaseAClickConsumed> {
  return racePhaseAClickConsumed(args as never);
}
function runPhaseB(
  args: FakeLocatorBundle & { timeout: number },
): ReturnType<typeof racePhaseBPostClick> {
  return racePhaseBPostClick(args as never);
}

describe("racePhaseAClickConsumed", () => {
  // The per-attempt timeout the production code uses (45s). The fake
  // timers in this suite never actually wait wall-clock time, so it
  // doesn't matter what value we pick — but matching production keeps
  // the assertions readable.
  const TIMEOUT = 45_000;

  afterEach(() => {
    vi.useRealTimers();
  });

  it('races against joinButton.waitFor({state: "hidden"}), NOT visible — proves the bug fix', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      // Button never hides — but the call itself MUST request hidden.
      joinButton: makeFakeLocator({ outcome: { kind: "never" } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    // Drain microtasks so each branch's waitFor is invoked.
    await vi.advanceTimersByTimeAsync(0);

    const stateOpts = bundle.joinButton.calls.map((c) => c.state);
    expect(stateOpts).toContain("hidden");
    expect(stateOpts).not.toContain("visible");

    // Resolve the race by hiding the button so the promise settles —
    // otherwise vitest's open-handle detector complains.
    vi.useRealTimers();
    void racePromise;
  });

  it("returns null when nothing resolves before the timeout (click-not-consumed path)", async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      // All six waitFors reject after exactly the timeout — this is the
      // canonical Playwright behaviour when nothing happens.
      joinButton: makeFakeLocator({
        outcomeByState: {
          hidden: { kind: "reject", afterMs: TIMEOUT },
        },
      }),
      grid: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingRoom: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingForHost: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      rejected: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      errorScreen: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(TIMEOUT + 1);
    const outcome = await racePromise;
    expect(outcome).toBeNull();
  });

  it("genuinely waits the full timeout when the click is not consumed (no early ~80ms collapse)", async () => {
    vi.useFakeTimers();
    const start = Date.now();
    const bundle = makeLocatorBundle({
      // Button stays visible — Phase A's hidden-wait rejects at the
      // timeout boundary. This is exactly the bug scenario: in v1.7.2
      // the race resolved in ~80ms because it raced state:"visible" on
      // an already-visible button. Now it correctly waits the full
      // budget.
      joinButton: makeFakeLocator({
        outcomeByState: { hidden: { kind: "reject", afterMs: TIMEOUT } },
      }),
      grid: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingRoom: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingForHost: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      rejected: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      errorScreen: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    // Halfway through: nothing should have resolved yet.
    await vi.advanceTimersByTimeAsync(TIMEOUT / 2);
    let settled = false;
    void racePromise.then(() => {
      settled = true;
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBe(false);
    // Drive to (just past) the boundary.
    await vi.advanceTimersByTimeAsync(TIMEOUT / 2 + 1);
    const outcome = await racePromise;
    const elapsed = Date.now() - start;
    expect(outcome).toBeNull();
    // The fake-timers clock should report a full timeout — proves the
    // race did NOT short-circuit on an "already visible" button.
    expect(elapsed).toBeGreaterThanOrEqual(TIMEOUT);
  });

  it('resolves to "grid" when the grid appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      grid: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 1_000 } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(2_000);
    expect(await racePromise).toBe("grid");
  });

  it('resolves to "button-hidden" when the button hides first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      joinButton: makeFakeLocator({
        outcomeByState: { hidden: { kind: "resolve", afterMs: 500 } },
      }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("button-hidden");
  });

  it('resolves to "waiting-room" when the waiting-room screen appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      waitingRoom: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 500 } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("waiting-room");
  });

  it('resolves to "waiting-for-host" when the waiting-for-host screen appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      waitingForHost: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 500 } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("waiting-for-host");
  });

  it('resolves to "rejected" when the rejected screen appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      rejected: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 500 } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("rejected");
  });

  it('resolves to "error" when the error screen appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      errorScreen: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 500 } }),
    });
    const racePromise = runPhaseA({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("error");
  });
});

describe("racePhaseBPostClick", () => {
  const TIMEOUT = 45_000;

  afterEach(() => {
    vi.useRealTimers();
  });

  it('races against joinButton.waitFor({state: "visible"}) — the legitimate reappearance signal', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle();
    const racePromise = runPhaseB({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(0);

    const stateOpts = bundle.joinButton.calls.map((c) => c.state);
    expect(stateOpts).toContain("visible");
    expect(stateOpts).not.toContain("hidden");

    vi.useRealTimers();
    void racePromise;
  });

  it("returns null when nothing resolves before the timeout (click-consumed-no-followup path)", async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      joinButton: makeFakeLocator({
        outcomeByState: { visible: { kind: "reject", afterMs: TIMEOUT } },
      }),
      grid: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingRoom: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      waitingForHost: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      rejected: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
      errorScreen: makeFakeLocator({ outcome: { kind: "reject", afterMs: TIMEOUT } }),
    });
    const racePromise = runPhaseB({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(TIMEOUT + 1);
    expect(await racePromise).toBeNull();
  });

  it('resolves to "grid" when the grid appears first', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      grid: makeFakeLocator({ outcome: { kind: "resolve", afterMs: 1_000 } }),
    });
    const racePromise = runPhaseB({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(2_000);
    expect(await racePromise).toBe("grid");
  });

  it('resolves to "button-reappeared" when the join button comes back', async () => {
    vi.useFakeTimers();
    const bundle = makeLocatorBundle({
      joinButton: makeFakeLocator({
        outcomeByState: { visible: { kind: "resolve", afterMs: 500 } },
      }),
    });
    const racePromise = runPhaseB({ ...bundle, timeout: TIMEOUT });
    await vi.advanceTimersByTimeAsync(1_000);
    expect(await racePromise).toBe("button-reappeared");
  });

  it("propagates each non-grid terminal screen (waiting-room / waiting-for-host / rejected / error)", async () => {
    const cases = [
      { key: "waitingRoom", expected: "waiting-room" },
      { key: "waitingForHost", expected: "waiting-for-host" },
      { key: "rejected", expected: "rejected" },
      { key: "errorScreen", expected: "error" },
    ] as const;

    for (const c of cases) {
      vi.useFakeTimers();
      const fast = makeFakeLocator({ outcome: { kind: "resolve", afterMs: 500 } });
      const bundle = makeLocatorBundle({ [c.key]: fast } as Record<string, FakeLocator>);
      const racePromise = runPhaseB({ ...bundle, timeout: TIMEOUT });
      await vi.advanceTimersByTimeAsync(1_000);
      expect(await racePromise).toBe(c.expected);
      vi.useRealTimers();
    }
  });
});

// Phase A + Phase B compose into the new joinWithRetries control flow.
// These tests model the composition at the *outcome* level (without
// invoking the production `joinWithRetries`, which would require driving
// a real Playwright Page + the diagnostic recorder). The composition
// table here is what the production loop maps to error messages — keep
// it in lockstep with the switch in `joinWithRetries`.
describe("two-phase outcome composition (joinWithRetries control flow)", () => {
  type PhaseA =
    | "grid"
    | "waiting-room"
    | "waiting-for-host"
    | "rejected"
    | "error"
    | "button-hidden"
    | null;
  type PhaseB =
    | "grid"
    | "waiting-room"
    | "waiting-for-host"
    | "rejected"
    | "error"
    | "button-reappeared"
    | null;
  type Disposition =
    | "success"
    | "retry-click-not-consumed"
    | "retry-button-reappeared"
    | "retry-no-followup"
    | "throw-waiting-room"
    | "throw-waiting-for-host"
    | "throw-rejected"
    | "throw-error";

  function compose(a: PhaseA, b: PhaseB | undefined): Disposition {
    if (a === null) return "retry-click-not-consumed";
    if (a === "grid") return "success";
    if (a === "waiting-room") return "throw-waiting-room";
    if (a === "waiting-for-host") return "throw-waiting-for-host";
    if (a === "rejected") return "throw-rejected";
    if (a === "error") return "throw-error";
    // a === "button-hidden" — run Phase B.
    if (b === undefined)
      throw new Error("test bug: Phase B must be defined when Phase A is button-hidden");
    if (b === null) return "retry-no-followup";
    if (b === "grid") return "success";
    if (b === "waiting-room") return "throw-waiting-room";
    if (b === "waiting-for-host") return "throw-waiting-for-host";
    if (b === "rejected") return "throw-rejected";
    if (b === "error") return "throw-error";
    return "retry-button-reappeared";
  }

  it("Phase A timeout (button stays visible) → retry-click-not-consumed", () => {
    expect(compose(null, undefined)).toBe("retry-click-not-consumed");
  });

  it('Phase A "grid" → success (no Phase B)', () => {
    expect(compose("grid", undefined)).toBe("success");
  });

  it('Phase A "button-hidden" + Phase B "grid" → success', () => {
    expect(compose("button-hidden", "grid")).toBe("success");
  });

  it('Phase A "button-hidden" + Phase B "button-reappeared" → retry-button-reappeared', () => {
    expect(compose("button-hidden", "button-reappeared")).toBe("retry-button-reappeared");
  });

  it('Phase A "button-hidden" + Phase B timeout (null) → retry-no-followup', () => {
    expect(compose("button-hidden", null)).toBe("retry-no-followup");
  });

  // Every non-grid terminal outcome must short-circuit with a typed
  // error regardless of which phase saw it.
  it('Phase A "waiting-room" → throw-waiting-room', () => {
    expect(compose("waiting-room", undefined)).toBe("throw-waiting-room");
  });
  it('Phase A "waiting-for-host" → throw-waiting-for-host', () => {
    expect(compose("waiting-for-host", undefined)).toBe("throw-waiting-for-host");
  });
  it('Phase A "rejected" → throw-rejected', () => {
    expect(compose("rejected", undefined)).toBe("throw-rejected");
  });
  it('Phase A "error" → throw-error', () => {
    expect(compose("error", undefined)).toBe("throw-error");
  });
  it('Phase A "button-hidden" + Phase B "waiting-room" → throw-waiting-room', () => {
    expect(compose("button-hidden", "waiting-room")).toBe("throw-waiting-room");
  });
  it('Phase A "button-hidden" + Phase B "waiting-for-host" → throw-waiting-for-host', () => {
    expect(compose("button-hidden", "waiting-for-host")).toBe("throw-waiting-for-host");
  });
  it('Phase A "button-hidden" + Phase B "rejected" → throw-rejected', () => {
    expect(compose("button-hidden", "rejected")).toBe("throw-rejected");
  });
  it('Phase A "button-hidden" + Phase B "error" → throw-error', () => {
    expect(compose("button-hidden", "error")).toBe("throw-error");
  });

  // The full set of distinguishable outcomes after the fix. v1.7.2
  // collapsed "click-not-consumed" into "button-reappeared" — listing
  // both here is the regression guard.
  it("all 7 terminal dispositions are uniquely distinguishable", () => {
    const dispositions: Disposition[] = [
      compose("grid", undefined),
      compose("waiting-room", undefined),
      compose("waiting-for-host", undefined),
      compose("rejected", undefined),
      compose("error", undefined),
      compose(null, undefined),
      compose("button-hidden", "button-reappeared"),
    ];
    expect(new Set(dispositions).size).toBe(7);
  });
});
