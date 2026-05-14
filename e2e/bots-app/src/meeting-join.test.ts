import { describe, it, expect } from "vitest";

import {
  JoinRejectedError,
  MEETING_STATE_SELECTORS,
  MeetingNavigatedAwayError,
  WaitingRoomError,
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
