import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";

import {
  type ClickAttemptDiagnostics,
  ALLOWED_DISPLAY_NAME_CHARS_RE,
  JoinRejectedError,
  MEETING_STATE_SELECTORS,
  MeetingNavigatedAwayError,
  WaitingRoomError,
  classifyJoinModeText,
  closePeerList,
  detectJoinMode,
  ensureDisplayNameInMeeting,
  enableJoinMedia,
  ensureWaitingRoomOff,
  installClickDiagnostics,
  isSessionUserIdResponse,
  JOIN_MEDIA_ON,
  JoinMediaNotAppliedError,
  type JoinMediaPage,
  joinMeetingAndEnableMedia,
  MEDIA_CLICK_TIMEOUT_MS,
  MEDIA_CONFIRM_MS,
  MEDIA_CONTROL_WAIT_MS,
  MEDIA_SETTLE_TIMEOUT_MS,
  MEDIA_TOGGLE_ATTEMPTS,
  logPostClickDiagnostics,
  sessionUserIdFromBody,
  waitForJoinButton,
} from "./meeting-join";
import {
  CAMERA_TOOLTIP,
  MIC_UNMUTE_SELECTOR,
  cameraButtonSelector,
  cameraControlSelector,
  micControlSelector,
  peerListControlSelector,
  unavailableOffSelector,
} from "./control-buttons";
import { UI_PROBE_TIME_LIMIT_MS } from "./time-limit";

// #865 regression lock (the deterministic one). The end-to-end Playwright
// spec (bot-join-flow.spec.ts) is smoke coverage of the real join flow but is
// NOT a #865 lock: reverting the gate reintroduces a Promise.race whose prompt
// arm still usually wins, so the spec false-passes (verified via mutation).
// This unit test pins the GATE directly: while the display-name form is still
// visible, the join-button race arm must NOT resolve. Flip
// `if (blockWhileFormsPresent)` to `if (false)` in meeting-join.ts and the
// first test below goes red — the property #2023 added to resolve #865.
describe("waitForJoinButton — #865 form-gating", () => {
  const resolvedWait = () => Promise.resolve();
  // A wait that never settles (the watched element never reaches the state).
  const pendingWait = () => new Promise<void>(() => {});
  const mkLocator = (waitFor: (opts: { state: string }) => Promise<void>) =>
    ({ waitFor: vi.fn(waitFor) }) as unknown as Parameters<typeof waitForJoinButton>[0];

  it("blocks the join-button arm while the display-name form is still visible", async () => {
    const joinButton = mkLocator(() => resolvedWait()); // Join button IS present…
    const homepageMeetingInput = mkLocator(() => resolvedWait()); // …homepage form gone…
    // …but the display-name prompt is STILL visible (never goes hidden).
    const displayNameInput = mkLocator(({ state }) =>
      state === "hidden" ? pendingWait() : resolvedWait(),
    );
    const outcome = await Promise.race([
      waitForJoinButton(
        joinButton,
        homepageMeetingInput,
        displayNameInput,
        10_000,
        false,
        true,
      ).then(() => "RESOLVED" as const),
      new Promise<"BLOCKED">((r) => setTimeout(() => r("BLOCKED"), 60)),
    ]);
    // With the #865 gate on, the arm must stay blocked so the prompt arm wins
    // the race and the display-name input is filled first. With the gate
    // reverted (`if (false)`), the join-button arm resolves and this is
    // "RESOLVED" — the mutation that makes this assertion fail.
    expect(outcome).toBe("BLOCKED");
  });

  it("resolves once both forms are hidden", async () => {
    const joinButton = mkLocator(() => resolvedWait());
    const homepageMeetingInput = mkLocator(() => resolvedWait());
    const displayNameInput = mkLocator(() => resolvedWait()); // hidden -> gate passes
    await expect(
      waitForJoinButton(joinButton, homepageMeetingInput, displayNameInput, 10_000, false, true),
    ).resolves.toBeUndefined();
  });
});

describe("meeting-join module surface", () => {
  it("exports joinMeetingAndEnableMedia as a function", () => {
    expect(typeof joinMeetingAndEnableMedia).toBe("function");
  });

  it("exports ensureDisplayNameInMeeting as a function", () => {
    expect(typeof ensureDisplayNameInMeeting).toBe("function");
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

describe("session user_id from the meeting-api participant-status responses", () => {
  it("matches only this meeting's join/status paths", () => {
    const base = "https://api.example.test/api/v1/meetings";
    for (const tail of ["join", "join-guest", "status", "guest-status"]) {
      expect(isSessionUserIdResponse(`${base}/m1/${tail}`, "m1")).toBe(true);
    }
    expect(isSessionUserIdResponse(`${base}/m2/join`, "m1")).toBe(false);
    expect(isSessionUserIdResponse(`${base}/m1/leave`, "m1")).toBe(false);
    expect(isSessionUserIdResponse(`${base}/m1/participants`, "m1")).toBe(false);
    expect(isSessionUserIdResponse("not a url", "m1")).toBe(false);
  });

  it("reads a non-empty result.user_id from a successful body only", () => {
    expect(sessionUserIdFromBody({ success: true, result: { user_id: "a@b.c" } })).toBe("a@b.c");
    expect(sessionUserIdFromBody({ success: true, result: { user_id: "" } })).toBeNull();
    expect(sessionUserIdFromBody({ success: false, result: { user_id: "a@b.c" } })).toBeNull();
    expect(sessionUserIdFromBody({ success: true, result: null })).toBeNull();
    expect(sessionUserIdFromBody(null)).toBeNull();
  });
});

describe("ALLOWED_DISPLAY_NAME_CHARS_RE", () => {
  // Locks the regex against the rules in
  // `videocall-types/src/validation.rs::is_allowed_display_name_char` —
  // ASCII letters, numbers, spaces, underscore, hyphen, apostrophe.
  // If those server-side rules change, this regex (and the dioxus UI's
  // `validate_display_name`) need to change together. The bot's
  // pre-check would otherwise let through values the meeting UI then
  // rejects, leaving the rename modal stuck.
  it("accepts the canonical allowed alphabet", () => {
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("Alice")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("ALICE")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("Alice 1")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice_1")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice-bob")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("O'Neil")).toBe(true);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("Bot alice")).toBe(true);
  });

  it("rejects the template literal that was the original bug-trigger", () => {
    // The user-reported case: typing `Bot {participant}` in the
    // single-bot displayName field. Without the server-side
    // templateDisplayName substitution (added alongside this regex),
    // the literal `{` and `}` reach the meeting UI's validator and
    // get rejected. The bot's pre-check must short-circuit before
    // typing — otherwise the modal opens but never closes.
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("Bot {participant}")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("{participant}")).toBe(false);
  });

  it("rejects other disallowed characters", () => {
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice@example.com")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice/bob")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice<bob>")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice!")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice.bob")).toBe(false);
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("alice🙂")).toBe(false);
  });

  it("rejects the empty string (no characters to validate)", () => {
    expect(ALLOWED_DISPLAY_NAME_CHARS_RE.test("")).toBe(false);
  });
});

describe("ensureDisplayNameInMeeting", () => {
  // The Playwright-driven branches (open peer list → click pencil →
  // fill → save) are covered by the manual smoke run described in
  // `bots-app/README.md`. Here we lock in the pure-control-flow early
  // returns so they don't silently regress: an empty displayName
  // argument must be a no-op (no UI interaction attempted), and a
  // displayName containing characters the meeting UI rejects (e.g.
  // an unsubstituted `{participant}` template) must short-circuit
  // BEFORE we open the peer list — otherwise the modal opens and
  // gets stuck on the inline validation error.
  it("returns immediately when displayName is empty (no page calls)", async () => {
    const mouseMove = vi.fn();
    const fakePage = {
      mouse: { move: mouseMove },
      waitForTimeout: vi.fn(),
      locator: vi.fn(),
    } as unknown as Parameters<typeof ensureDisplayNameInMeeting>[0]["page"];

    await ensureDisplayNameInMeeting({
      page: fakePage,
      participant: "alice",
      displayName: "",
    });

    expect(mouseMove).not.toHaveBeenCalled();
    expect(
      (fakePage as unknown as { locator: ReturnType<typeof vi.fn> }).locator,
    ).not.toHaveBeenCalled();
  });

  it("treats whitespace-only displayName as empty (no-op)", async () => {
    const mouseMove = vi.fn();
    const fakePage = {
      mouse: { move: mouseMove },
      waitForTimeout: vi.fn(),
      locator: vi.fn(),
    } as unknown as Parameters<typeof ensureDisplayNameInMeeting>[0]["page"];

    await ensureDisplayNameInMeeting({
      page: fakePage,
      participant: "alice",
      displayName: "   ",
    });

    expect(mouseMove).not.toHaveBeenCalled();
  });

  it("returns without touching the page when displayName contains invalid chars", async () => {
    // The user-reported failure path: bot received a literal
    // `Bot {participant}` (server-side substitution didn't happen).
    // The pre-check must short-circuit BEFORE opening the peer list
    // — otherwise the modal would open with the invalid value typed
    // in, validation would reject it, and the modal would stay open
    // blocking the bot's subsequent steps.
    const mouseMove = vi.fn();
    const fakePage = {
      mouse: { move: mouseMove },
      waitForTimeout: vi.fn(),
      locator: vi.fn(),
    } as unknown as Parameters<typeof ensureDisplayNameInMeeting>[0]["page"];

    await ensureDisplayNameInMeeting({
      page: fakePage,
      participant: "alice",
      displayName: "Bot {participant}",
    });

    expect(mouseMove).not.toHaveBeenCalled();
    expect(
      (fakePage as unknown as { locator: ReturnType<typeof vi.fn> }).locator,
    ).not.toHaveBeenCalled();
  });

  describe("on a renderer that never answers its untimed calls", () => {
    afterEach(() => vi.useRealTimers());
    const never = (): Promise<never> => new Promise(() => {});
    const renamePage = (hang: { inputs: boolean; visibility: boolean }) => {
      const loc: Record<string, unknown> = {
        isVisible: hang.visibility ? never : async () => true,
        click: async () => {},
        fill: async () => {},
        pressSequentially: async () => {},
        textContent: async () => "Old (You)",
        waitFor: async (o?: { state?: string }) => {
          if (o?.state === "hidden") throw new Error("modal still open");
        },
      };
      for (const k of ["filter", "first", "last", "locator", "getByRole"]) loc[k] = () => loc;
      const move = vi.fn(hang.inputs ? never : async () => {});
      const press = vi.fn(hang.inputs ? never : async () => {});
      const page = {
        locator: () => loc,
        mouse: { move },
        keyboard: { press },
        waitForTimeout: async () => {},
      } as unknown as Parameters<typeof ensureDisplayNameInMeeting>[0]["page"];
      return { page, move, press };
    };

    it.each([
      ["mouse and key inputs", { inputs: true, visibility: false }, 3],
      ["visibility reads", { inputs: false, visibility: true }, 1],
    ])("still finishes the rename when its %s never settle", async (_c, hang, moves) => {
      vi.useFakeTimers();
      const { page, move, press } = renamePage(hang);
      let finished = false;
      void ensureDisplayNameInMeeting({ page, participant: "alice", displayName: "New" }).then(
        () => (finished = true),
      );
      await vi.advanceTimersByTimeAsync(6 * UI_PROBE_TIME_LIMIT_MS);
      expect(finished).toBe(true);
      expect(move.mock.calls.length + press.mock.calls.length).toBe(moves);
    });
  });

  describe("on a renderer whose untimed calls reject", () => {
    afterEach(() => vi.useRealTimers());
    const gone = (): Promise<never> => Promise.reject(new Error("Target closed"));

    it.each([
      ["mouse moves", "move", 2, 1],
      ["key presses", "press", 2, 1],
      ["visibility reads", "isVisible", 1, 0],
    ] as const)(
      "still finishes the rename when its %s reject",
      async (_c, fails, moves, presses) => {
        vi.useFakeTimers();
        const loc: Record<string, unknown> = {
          isVisible: fails === "isVisible" ? gone : async () => true,
          click: async () => {},
          fill: async () => {},
          pressSequentially: async () => {},
          textContent: async () => "Old (You)",
          waitFor: async (o?: { state?: string }) => {
            if (o?.state === "hidden") throw new Error("modal still open");
          },
        };
        for (const k of ["filter", "first", "last", "locator", "getByRole"]) loc[k] = () => loc;
        const move = vi.fn(fails === "move" ? gone : async () => {});
        const press = vi.fn(fails === "press" ? gone : async () => {});
        const page = {
          locator: () => loc,
          mouse: { move },
          keyboard: { press },
          waitForTimeout: async () => {},
        } as unknown as Parameters<typeof ensureDisplayNameInMeeting>[0]["page"];
        const rename = ensureDisplayNameInMeeting({
          page,
          participant: "alice",
          displayName: "New",
        });
        await vi.advanceTimersByTimeAsync(0);
        await expect(rename).resolves.toBeUndefined();
        expect(move).toHaveBeenCalledTimes(moves);
        expect(press).toHaveBeenCalledTimes(presses);
      },
    );
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

type JoinHarnessState = "grid" | "waiting-room" | "rejected" | "button" | "prompt";

function makeJoinHarness(initialState: JoinHarnessState): {
  page: Parameters<typeof joinMeetingAndEnableMedia>[0]["page"];
  joinClick: ReturnType<typeof vi.fn>;
  promptFill: ReturnType<typeof vi.fn>;
} {
  let state = initialState;
  let staleButtonObservation = false;
  const waiters = new Set<() => void>();
  const joinClick = vi.fn(async () => {
    if (state === "grid") {
      await new Promise((resolve) => setTimeout(resolve, 0));
      throw new Error("element is detached from the DOM");
    }
    state = "grid";
    for (const notify of waiters) notify();
  });
  const promptFill = vi.fn(async () => {
    state = "grid";
    staleButtonObservation = true;
    for (const notify of waiters) notify();
  });

  function locator(kind: JoinHarnessState | "other" | "open-peers") {
    const matches = (requestedState: string | undefined): boolean => {
      if (kind === "button") {
        if (requestedState !== "hidden" && staleButtonObservation) return true;
        return requestedState === "hidden" ? state !== "button" : state === "button";
      }
      return requestedState === "hidden" ? state !== kind : state === kind;
    };
    const result = {
      first: vi.fn().mockReturnThis(),
      last: vi.fn().mockReturnThis(),
      and: vi.fn().mockReturnThis(),
      filter: vi.fn().mockReturnThis(),
      locator: vi.fn(() => locator("other")),
      getByRole: vi.fn(() => locator("other")),
      waitFor: vi.fn(
        (options?: { state?: string }): Promise<void> =>
          new Promise((resolve) => {
            const check = (): void => {
              if (matches(options?.state)) {
                waiters.delete(check);
                resolve();
              }
            };
            waiters.add(check);
            check();
          }),
      ),
      isVisible: vi.fn(async () => matches("visible")),
      innerText: vi.fn(async () => (kind === "button" ? "Join Meeting" : "")),
      textContent: vi.fn(async () => ""),
      click:
        kind === "button"
          ? joinClick
          : kind === "open-peers"
            ? vi.fn(async () => {
                throw new Error("peer list unavailable in join harness");
              })
            : vi.fn(async () => undefined),
      hover: vi.fn(async () => undefined),
      fill: vi.fn(async () => undefined),
      press: vi.fn(async () => undefined),
      pressSequentially: kind === "prompt" ? promptFill : vi.fn(async () => undefined),
      getAttribute: vi.fn(async () => null),
    };
    return result;
  }

  const locators = new Map<string, ReturnType<typeof locator>>([
    ["#grid-container", locator("grid")],
    ['input[placeholder="Enter your display name"]', locator("prompt")],
    [MEETING_STATE_SELECTORS.waitingRoom, locator("waiting-room")],
    [MEETING_STATE_SELECTORS.rejected, locator("rejected")],
  ]);
  const other = locator("other");
  const openPeers = locator("open-peers");
  const button = locator("button");
  const media = mediaPage([MIC_UNMUTE_SELECTOR, cameraControlSelector("off")]).page;
  const page = {
    locator: vi.fn((selector: string) =>
      selector === peerListControlSelector("off")
        ? openPeers
        : selector.includes("-toggle-button")
          ? media.locator(selector)
          : (locators.get(selector) ?? other),
    ),
    getByRole: vi.fn(() => button),
    on: vi.fn(),
    off: vi.fn(),
    url: vi.fn(() => "https://example.test/meeting/TestRoom"),
    waitForTimeout: vi.fn(async () => undefined),
    mouse: { move: vi.fn(async () => undefined) },
    keyboard: { press: vi.fn(async () => undefined) },
  };

  return {
    page: page as unknown as Parameters<typeof joinMeetingAndEnableMedia>[0]["page"],
    joinClick,
    promptFill,
  };
}

/** The harness renders both controls off, so the default join turns both on. */
const JOINED_MEDIA = { camera: true, mic: true };

const MIC_ON = micControlSelector("on");
const CAM_ON = cameraControlSelector("on");
const TOGGLES = new Map([
  [MIC_UNMUTE_SELECTOR, MIC_ON],
  [MIC_ON, MIC_UNMUTE_SELECTOR],
  [cameraControlSelector("off"), CAM_ON],
  [CAM_ON, cameraControlSelector("off")],
]);

interface BarOptions {
  /** Virtual ms at which the controls first render. */
  renderAtMs?: number;
  /** Clicks that time out without landing, as on a CPU-starved renderer. */
  failedClicks?: number;
  /** Clicks that land and then still report a timeout. */
  landedThenTimedOut?: number;
  /** Virtual ms a click needs to become actionable; a shorter timeout never lands it. */
  clickLatencyMs?: number;
  /** No click ever flips the control. A landed click otherwise toggles it, as the product's button does. */
  stuck?: boolean;
  /** Virtual ms from a landed click to the control's label flip; per landing, the last repeating. */
  flipAfterMs?: number | readonly number[];
  /** Virtual ms after the first flip at which it reverts, as a failed encoder start does. */
  revertFirstAfterMs?: number;
}

/**
 * An action bar on a virtual clock: `waitForTimeout` and expiring `waitFor`/`click`
 * timeouts advance it, and a landed click swaps the medium's control as the product does.
 */
function mediaPage(
  initial: readonly string[],
  opts: BarOptions = {},
): { page: JoinMediaPage; clicked: string[]; visible: Set<string>; now: () => number } {
  let now = 0;
  let failedClicks = opts.failedClicks ?? 0;
  let landedThenTimedOut = opts.landedThenTimedOut ?? 0;
  const visible = new Set<string>();
  const pending: Array<{ at: number; apply: () => void }> = [];
  const schedule = (at: number, apply: () => void): void => {
    pending.push({ at, apply });
    pending.sort((a, b) => a.at - b.at);
  };
  const tick = (): void => {
    while (pending.length > 0 && pending[0].at <= now) pending.shift()?.apply();
  };
  const shows = (sel: string): boolean => {
    tick();
    return sel.split(", ").some((part) => visible.has(part));
  };
  /** Advances to the first moment `sel` shows within `timeout`, or throws at the deadline. */
  const waitUntilShown = (sel: string, timeout: number, what: string): void => {
    const deadline = now + timeout;
    while (!shows(sel)) {
      const next = pending[0];
      if (next === undefined || next.at > deadline) {
        now = deadline;
        throw new Error(`locator.${what}: Timeout ${timeout}ms exceeded.`);
      }
      now = next.at;
    }
  };
  schedule(opts.renderAtMs ?? 0, () => initial.forEach((sel) => visible.add(sel)));
  const clicked: string[] = [];
  const page = {
    locator: vi.fn((sel: string) => {
      const loc = {
        first: () => loc,
        hover: vi.fn(async () => undefined),
        isVisible: vi.fn(async () => shows(sel)),
        click: vi.fn(async ({ timeout = 0 }: { timeout?: number } = {}) => {
          const latency = opts.clickLatencyMs ?? 0;
          if (failedClicks > 0 || timeout < latency) {
            failedClicks = Math.max(0, failedClicks - 1);
            now += timeout;
            throw new Error(`locator.click: Timeout ${timeout}ms exceeded.`);
          }
          waitUntilShown(sel, timeout - latency, "click");
          now += latency;
          clicked.push(sel);
          const next = TOGGLES.get(sel);
          if (opts.stuck || next === undefined) return;
          const delays = [opts.flipAfterMs ?? 0].flat();
          const flipAt = now + delays[Math.min(clicked.length - 1, delays.length - 1)];
          schedule(flipAt, () => {
            const [from, to] = visible.has(sel) ? [sel, next] : [next, sel];
            visible.delete(from);
            visible.add(to);
          });
          if (opts.revertFirstAfterMs !== undefined && clicked.length === 1) {
            schedule(flipAt + opts.revertFirstAfterMs, () => {
              visible.delete(next);
              visible.add(sel);
            });
          }
          if (landedThenTimedOut > 0) {
            landedThenTimedOut--;
            now += timeout;
            throw new Error(`locator.click: Timeout ${timeout}ms exceeded.`);
          }
        }),
        waitFor: vi.fn(async ({ timeout = 0 }: { timeout?: number } = {}) =>
          waitUntilShown(sel, timeout, "waitFor"),
        ),
      };
      return loc;
    }),
    waitForTimeout: vi.fn(async (ms: number) => {
      now += ms;
    }),
  };
  return { page: page as unknown as JoinMediaPage, clicked, visible, now: () => now };
}

describe("enableJoinMedia (#2914)", () => {
  const MIC = MIC_UNMUTE_SELECTOR;
  const CAM = cameraButtonSelector(CAMERA_TOOLTIP.off);
  it("turns both on by default and reports them on", async () => {
    const { page, clicked, now } = mediaPage([MIC, CAM]);
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([MIC, CAM]);
    expect(now()).toBe(2 * MEDIA_CONFIRM_MS);
  });

  it("leaves an opted-off medium untouched and reports it off only when its enable control shows", async () => {
    const { page, clicked } = mediaPage([MIC, CAM]);
    await expect(enableJoinMedia(page, "alice", { camera: false, mic: true })).resolves.toEqual({
      camera: false,
      mic: true,
    });
    expect(clicked).toEqual([MIC]);
  });

  it("turns off a medium the product joined on when it was requested off (storage-state prefs)", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM_ON]);
    await expect(enableJoinMedia(page, "alice", { camera: false, mic: false })).resolves.toEqual({
      camera: false,
      mic: false,
    });
    expect(clicked).toEqual([MIC_ON, CAM_ON]);
  });

  it("waits for an async enable to flip the control before reading it back", async () => {
    const { page, clicked } = mediaPage([MIC, CAM], { flipAfterMs: 50 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([MIC, CAM]);
  });

  it("retries a click that timed out on a starved renderer until the camera reads on", async () => {
    const { page, clicked, now } = mediaPage([MIC_ON, CAM], { failedClicks: 1 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM]);
    expect(now()).toBeGreaterThanOrEqual(MEDIA_CLICK_TIMEOUT_MS);
  });

  it("accepts a click that lands on the last attempt", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], {
      failedClicks: MEDIA_TOGGLE_ATTEMPTS - 1,
    });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM]);
  });

  it("gives a control that is slow to become actionable the whole click budget", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], { clickLatencyMs: 4_000 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM]);
  });

  it("never clicks an off-requested control that renders after the first wait", async () => {
    const { page, clicked } = mediaPage([MIC, CAM], { renderAtMs: 12_000 });
    await expect(enableJoinMedia(page, "alice", { camera: false, mic: false })).resolves.toEqual({
      camera: false,
      mic: false,
    });
    expect(clicked).toEqual([]);
  });

  it("waits for controls that render late instead of reporting them unknown", async () => {
    const { page, clicked } = mediaPage([MIC, CAM], { renderAtMs: 4_000 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([MIC, CAM]);
  });

  it("does not click again when a landed click's label flips after the settle wait", async () => {
    const flipAfterMs = MEDIA_SETTLE_TIMEOUT_MS + 1_500;
    const { page, clicked } = mediaPage([MIC_ON, CAM], { flipAfterMs });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM]);
  });

  it("clicks again when the camera reads on but its encoder start turns it back off", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], { revertFirstAfterMs: 500 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM, CAM]);
  });

  it("does not click a landed click's control back while its label is still unflipped", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], { flipAfterMs: 25_000 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM]);
  });

  it("fails with the observed state when the camera never takes", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], { stuck: true });
    const err = await enableJoinMedia(page, "alice", JOIN_MEDIA_ON).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(JoinMediaNotAppliedError);
    expect(err).toMatchObject({ medium: "camera", requested: true, observed: false });
    expect(String(err)).toContain("camera requested on but reads off");
    expect(clicked).toEqual([CAM]);
  });

  it("backs off 1 s, then 2 s, between attempts", async () => {
    const { page } = mediaPage([MIC_ON, CAM], { stuck: true });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).rejects.toBeInstanceOf(
      JoinMediaNotAppliedError,
    );
    const waits = vi.mocked(page.waitForTimeout).mock.calls.map(([ms]) => ms);
    const C = MEDIA_CONFIRM_MS;
    expect(waits).toEqual([C, C, 1_000, C, 2_000, C]);
  });

  it("fails when a medium turned off by the bot stays on", async () => {
    const { page } = mediaPage([MIC_ON, CAM], { stuck: true });
    await expect(
      enableJoinMedia(page, "alice", { camera: false, mic: false }),
    ).rejects.toMatchObject({ medium: "microphone", requested: false, observed: true });
  });

  it("fails an on request whose control never renders", async () => {
    const { page, clicked } = mediaPage([]);
    await expect(
      enableJoinMedia(page, "alice", { camera: false, mic: true }),
    ).rejects.toMatchObject({ medium: "microphone", requested: true, observed: null });
    expect(clicked).toEqual([]);
  });

  it("fails an off request whose control never renders, since replayed preferences can join it on", async () => {
    const { page, clicked } = mediaPage([]);
    await expect(
      enableJoinMedia(page, "alice", { camera: false, mic: false }),
    ).rejects.toMatchObject({ medium: "microphone", requested: false, observed: null });
    expect(clicked).toEqual([]);
  });

  it("meets an off request on a device that is unavailable and off, without clicking it, and says so", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { page, clicked } = mediaPage([
      unavailableOffSelector("mic"),
      unavailableOffSelector("camera"),
    ]);
    await expect(enableJoinMedia(page, "alice", { camera: false, mic: false })).resolves.toEqual({
      camera: false,
      mic: false,
    });
    expect(clicked).toEqual([]);
    const lines = warn.mock.calls.map((a) => a.join(" "));
    warn.mockRestore();
    expect(lines.filter((l) => l.includes("device is unavailable"))).toEqual([
      "[alice] microphone off as requested, but its device is unavailable",
      "[alice] camera off as requested, but its device is unavailable",
    ]);
  });

  it("warns of an unavailable device only for the medium requested off", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { page } = mediaPage([unavailableOffSelector("mic"), CAM_ON]);
    await expect(enableJoinMedia(page, "alice", { camera: true, mic: false })).resolves.toEqual({
      camera: true,
      mic: false,
    });
    const lines = warn.mock.calls.map((a) => a.join(" "));
    warn.mockRestore();
    expect(lines.filter((l) => l.includes("device is unavailable"))).toEqual([
      "[alice] microphone off as requested, but its device is unavailable",
    ]);
  });

  it("still fails an on request on an unavailable device", async () => {
    const { page } = mediaPage([MIC_ON, unavailableOffSelector("camera")]);
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).rejects.toMatchObject({
      medium: "camera",
      requested: true,
      observed: null,
    });
  });

  it("waits out a click that timed out after landing instead of clicking it back", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], {
      landedThenTimedOut: 1,
      flipAfterMs: MEDIA_CLICK_TIMEOUT_MS + 5_000,
    });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    await page.waitForTimeout(2 * MEDIA_SETTLE_TIMEOUT_MS);
    expect(clicked).toEqual([CAM]);
    await expect(page.locator(CAM_ON).isVisible()).resolves.toBe(true);
  });

  it("outlasts a timed-out click given up on, so its late landing cannot undo a success", async () => {
    const { page } = mediaPage([MIC_ON, CAM], {
      landedThenTimedOut: 1,
      flipAfterMs: [MEDIA_CLICK_TIMEOUT_MS + MEDIA_SETTLE_TIMEOUT_MS + 5_000, 0],
    });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    await page.waitForTimeout(3 * MEDIA_SETTLE_TIMEOUT_MS);
    await expect(page.locator(CAM_ON).isVisible()).resolves.toBe(true);
  });

  it("clicks again after an on that appeared during a backoff reverted", async () => {
    const { page, clicked } = mediaPage([MIC_ON, CAM], {
      flipAfterMs: [11_500, 0],
      revertFirstAfterMs: 600,
    });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).resolves.toEqual({
      camera: true,
      mic: true,
    });
    expect(clicked).toEqual([CAM, CAM]);
  });

  it("confirms an on that appeared during a backoff, and fails when it reverts", async () => {
    const { page } = mediaPage([MIC_ON, CAM], { flipAfterMs: 11_500, revertFirstAfterMs: 600 });
    await expect(enableJoinMedia(page, "alice", JOIN_MEDIA_ON)).rejects.toMatchObject({
      medium: "camera",
      requested: true,
      observed: false,
    });
  });

  it("is what joinMeetingAndEnableMedia runs with its media argument", async () => {
    const { page } = makeJoinHarness("button");
    const { page: controls, clicked } = mediaPage([MIC, CAM]);
    const base = page.locator.bind(page);
    (page as unknown as { locator: (s: string) => unknown }).locator = (s: string) =>
      s.includes("-toggle-button") ? controls.locator(s) : base(s);
    await expect(
      joinMeetingAndEnableMedia({
        page,
        participant: "alice",
        displayName: "",
        meetingId: "TestRoom",
        media: { camera: false, mic: false },
      }),
    ).resolves.toEqual({ camera: false, mic: false });
    expect(clicked).toEqual([]);
  });
});

describe("joinMeetingAndEnableMedia state machine", () => {
  const args = {
    participant: "alice",
    displayName: "",
    meetingId: "TestRoom",
  };

  it("accepts auto-join when filling the display-name prompt transitions to the grid", async () => {
    const { page, joinClick, promptFill } = makeJoinHarness("prompt");
    const join = joinMeetingAndEnableMedia({
      page,
      ...args,
      displayName: "Alice",
    });
    const boundedJoin = Promise.race([
      join,
      new Promise<never>((_resolve, reject) => {
        setTimeout(() => reject(new Error("auto-join did not resolve")), 500);
      }),
    ]);

    await expect(boundedJoin).resolves.toEqual(JOINED_MEDIA);
    expect(promptFill).toHaveBeenCalledWith("Alice", {
      delay: 30,
      timeout: 5_000,
    });
    expect(joinClick).not.toHaveBeenCalled();
  });

  // Call-site guards: revert the consumer to an unscoped/tooltip locator and
  // these fail, which the builders' own drift locks cannot see.
  it("drives the mic, camera and peer-list controls by their scoped selectors", async () => {
    const { page } = makeJoinHarness("prompt");
    const boundedJoin = Promise.race([
      joinMeetingAndEnableMedia({ page, ...args, displayName: "Alice" }),
      new Promise<never>((_resolve, reject) => {
        setTimeout(() => reject(new Error("auto-join did not resolve")), 500);
      }),
    ]);
    await expect(boundedJoin).resolves.toEqual(JOINED_MEDIA);

    const requested = (page.locator as unknown as { mock: { calls: unknown[][] } }).mock.calls.map(
      (c) => c[0],
    );
    expect(requested).toContain(MIC_UNMUTE_SELECTOR);
    expect(requested).toContain(cameraButtonSelector(CAMERA_TOOLTIP.off));
    expect(requested).toContain(peerListControlSelector("off"));
  });

  it("closePeerList targets the open-state peer-list selector", async () => {
    const seen: string[] = [];
    const btn = {
      isVisible: vi.fn(async () => true),
      click: vi.fn(async () => undefined),
      waitFor: vi.fn(async () => undefined),
    };
    const page = {
      locator: vi.fn((selector: string) => {
        seen.push(selector);
        return btn;
      }),
      mouse: { move: vi.fn(async () => undefined) },
      waitForTimeout: vi.fn(async () => undefined),
    };
    await closePeerList(page as unknown as Parameters<typeof closePeerList>[0], "alice");
    expect([...new Set(seen)]).toEqual([peerListControlSelector("on")]);
    expect(btn.click).toHaveBeenCalledTimes(1);
    expect(btn.waitFor).toHaveBeenCalledWith({ state: "hidden", timeout: 5_000 });
  });

  it("propagates WaitingRoomError from the real join path", async () => {
    const { page } = makeJoinHarness("waiting-room");
    await expect(joinMeetingAndEnableMedia({ page, ...args })).rejects.toBeInstanceOf(
      WaitingRoomError,
    );
  });

  it("propagates JoinRejectedError from the real join path", async () => {
    const { page } = makeJoinHarness("rejected");
    await expect(joinMeetingAndEnableMedia({ page, ...args })).rejects.toBeInstanceOf(
      JoinRejectedError,
    );
  });

  it("reports a hang-up during the media toggle as a navigation, not a media failure", async () => {
    const { page } = makeJoinHarness("button");
    const { page: controls, now } = mediaPage([]);
    const base = page.locator.bind(page);
    const on = page.on as unknown as { mock: { calls: Array<[string, (f: unknown) => void]> } };
    (page as unknown as { locator: (s: string) => unknown }).locator = (s: string) => {
      if (!s.includes("-toggle-button")) return base(s);
      const navigate = on.mock.calls.find(([ev]) => ev === "framenavigated")?.[1];
      navigate?.({ parentFrame: () => null, url: () => "https://example.test/" });
      return controls.locator(s);
    };
    await expect(joinMeetingAndEnableMedia({ page, ...args })).rejects.toBeInstanceOf(
      MeetingNavigatedAwayError,
    );
    expect(now()).toBeLessThan(2 * MEDIA_CONTROL_WAIT_MS);
  });

  it("stops the media retries once the launch is aborted", async () => {
    const { page } = makeJoinHarness("button");
    const { page: controls, now } = mediaPage([]);
    const abort = new AbortController();
    const base = page.locator.bind(page);
    (page as unknown as { locator: (s: string) => unknown }).locator = (s: string) => {
      if (!s.includes("-toggle-button")) return base(s);
      abort.abort();
      return controls.locator(s);
    };
    await expect(
      joinMeetingAndEnableMedia({ page, ...args, signal: abort.signal }),
    ).rejects.toBeInstanceOf(JoinMediaNotAppliedError);
    expect(now()).toBeLessThan(2 * MEDIA_CONTROL_WAIT_MS);
  });

  it("clicks the normal Join button and succeeds when the grid follows", async () => {
    const { page, joinClick } = makeJoinHarness("button");
    await expect(joinMeetingAndEnableMedia({ page, ...args })).resolves.toEqual(JOINED_MEDIA);
    expect(joinClick).toHaveBeenCalledTimes(1);
    expect(joinClick).toHaveBeenCalledWith({ timeout: 5_000 });
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

describe("joinMeetingAndEnableMedia onInMeeting", () => {
  it("calls onInMeeting once the grid is visible, before the rename and media steps", async () => {
    const { page } = makeJoinHarness("prompt");
    const asked = (page.locator as unknown as { mock: { calls: unknown[][] } }).mock.calls;
    const seenAtHook: unknown[][] = [];
    await joinMeetingAndEnableMedia({
      page,
      participant: "alice",
      meetingId: "TestRoom",
      displayName: "Alice",
      onInMeeting: async () => void seenAtHook.push(...asked.map((c) => [...c])),
    });
    const selectors = (calls: unknown[][]): unknown[] => calls.map((c) => c[0]);
    expect(selectors(seenAtHook)).toContain("#grid-container");
    for (const later of [peerListControlSelector("off"), MIC_UNMUTE_SELECTOR]) {
      expect(selectors(seenAtHook)).not.toContain(later);
      expect(selectors(asked)).toContain(later);
    }
  });
});

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

describe("a stalled renderer cannot hang the join's UI probes", () => {
  const NEVER = (): Promise<never> => new Promise<never>(() => {});
  const PENDING = Symbol("pending");
  let quiet: ReturnType<typeof vi.spyOn>[];
  beforeEach(() => {
    vi.useFakeTimers();
    quiet = [
      vi.spyOn(console, "log").mockImplementation(() => {}),
      vi.spyOn(console, "warn").mockImplementation(() => {}),
    ];
  });
  afterEach(() => {
    for (const spy of quiet) spy.mockRestore();
    vi.useRealTimers();
  });
  const timeoutWarnings = (): string[] =>
    (quiet[1].mock.calls as unknown[][])
      .map((c) => String(c[0]))
      .filter((m) => m.includes("did not settle"));
  async function after(work: Promise<unknown>, ms: number): Promise<unknown> {
    const settled = work.then(
      (v) => v,
      (e: unknown) => e,
    );
    await vi.advanceTimersByTimeAsync(ms);
    return Promise.race([settled, Promise.resolve(PENDING)]);
  }
  function stubPage(
    isVisible: (sel: string) => Promise<boolean>,
    mouseMove = vi.fn(async () => {}),
  ) {
    const click = vi.fn(async () => {});
    const page = {
      locator: vi.fn((sel: string) => {
        const l = {
          first: () => l,
          hover: vi.fn(async () => {}),
          isVisible: () => isVisible(sel),
          click,
          waitFor: vi.fn(async () => {
            throw new Error("not visible");
          }),
        };
        return l;
      }),
      waitForTimeout: vi.fn(async () => {}),
      mouse: { move: mouseMove },
    };
    return { page, click };
  }

  it("enableJoinMedia fails as unknown media once each probe's limit passes", async () => {
    const { page, click } = stubPage(NEVER);
    const probes = 2 * MEDIA_TOGGLE_ATTEMPTS;
    const out = await after(
      enableJoinMedia(page as unknown as JoinMediaPage, "alice", JOIN_MEDIA_ON),
      probes * UI_PROBE_TIME_LIMIT_MS,
    );
    expect(out).toBeInstanceOf(JoinMediaNotAppliedError);
    expect(out).toMatchObject({ medium: "microphone", requested: true, observed: null });
    expect(click).not.toHaveBeenCalled();
    expect(timeoutWarnings()).toHaveLength(probes);
  });

  it("enableJoinMedia does not click a control whose state probe stalled", async () => {
    const { page, click } = stubPage(async (sel) =>
      sel === MIC_UNMUTE_SELECTOR ? NEVER() : false,
    );
    const out = await after(
      enableJoinMedia(page as unknown as JoinMediaPage, "alice", { camera: false, mic: true }),
      MEDIA_TOGGLE_ATTEMPTS * UI_PROBE_TIME_LIMIT_MS,
    );
    expect(out).toMatchObject({ medium: "microphone", requested: true, observed: null });
    expect(click).not.toHaveBeenCalled();
    expect(timeoutWarnings()).toEqual(
      Array(MEDIA_TOGGLE_ATTEMPTS).fill(expect.stringContaining(MIC_UNMUTE_SELECTOR)),
    );
  });

  it("ensureWaitingRoomOff moves on when the toggle probe stalls", async () => {
    const toggle = { first: () => toggle, isVisible: NEVER, getAttribute: vi.fn() };
    const row = { locator: () => toggle, filter: () => row };
    const out = await after(
      ensureWaitingRoomOff({ locator: () => row } as never, "bot-1"),
      UI_PROBE_TIME_LIMIT_MS,
    );
    expect(out).toBeUndefined();
    expect(toggle.getAttribute).not.toHaveBeenCalled();
    expect(timeoutWarnings()).toHaveLength(1);
  });

  it.each([
    [
      "ensureDisplayNameInMeeting",
      (page: never) =>
        ensureDisplayNameInMeeting({ page, participant: "alice", displayName: "Alice Bot" }),
    ],
    ["closePeerList", (page: never) => closePeerList(page, "alice")],
  ] as const)("%s moves on when the mouse nudge stalls", async (_what, run) => {
    const { page } = stubPage(async () => false, vi.fn(NEVER));
    expect(await after(run(page as never), UI_PROBE_TIME_LIMIT_MS)).toBeUndefined();
  });

  function renamePage(failAt: "save" | "open") {
    const press = vi.fn(NEVER);
    const stub = (path: string): Record<string, unknown> => {
      const self: Record<string, unknown> = {
        first: () => self,
        last: () => self,
        filter: () => self,
        locator: (sel: string) => stub(`${path} ${sel}`),
        getByRole: (_role: string, o?: { name?: string }) => stub(`${path} role=${o?.name}`),
        isVisible: async () => true,
        click: async () => {},
        textContent: async () => "Old Name (You)",
        fill: async () => {},
        pressSequentially: async () => {},
        waitFor: async (o?: { state?: string }) => {
          if (!path.includes("input.input-apple")) return;
          if (o?.state === "hidden" || failAt === "open") throw new Error("modal stuck");
        },
      };
      return self;
    };
    const page = {
      locator: (sel: string) => stub(sel),
      mouse: { move: async () => {} },
      keyboard: { press },
      waitForTimeout: async () => {},
    };
    return { page, press };
  }

  it.each(["save", "open"] as const)(
    "ensureDisplayNameInMeeting moves on when the Escape after a stuck rename modal (%s) stalls",
    async (failAt) => {
      const { page, press } = renamePage(failAt);
      const out = await after(
        ensureDisplayNameInMeeting({
          page: page as never,
          participant: "alice",
          displayName: "Alice Bot",
        }),
        UI_PROBE_TIME_LIMIT_MS,
      );
      expect(out).toBeUndefined();
      expect(press).toHaveBeenCalledExactlyOnceWith("Escape");
    },
  );
});
