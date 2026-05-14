import { describe, expect, it } from "vitest";

import { badgeForBot, STATUS_BADGE_CLASS } from "../lib/constants";

describe("badgeForBot status mapping", () => {
  it("returns a friendly label for in-progress bots", () => {
    expect(badgeForBot({ status: "in-meeting" })).toEqual({
      label: "In meeting",
      badgeKey: "in-meeting",
    });
  });

  it("returns friendly labels for each known status key", () => {
    expect(badgeForBot({ status: "launching" })).toEqual({
      label: "Launching",
      badgeKey: "launching",
    });
    expect(badgeForBot({ status: "joining" })).toEqual({
      label: "Joining",
      badgeKey: "joining",
    });
    expect(badgeForBot({ status: "leaving" })).toEqual({
      label: "Leaving",
      badgeKey: "leaving",
    });
  });

  it("returns 'Done' for bots that exited via ttl-expired", () => {
    expect(badgeForBot({ status: "done", finishReason: "ttl-expired" })).toEqual({
      label: "Done",
      badgeKey: "done",
    });
  });

  it("returns 'Done (waiting)' for waiting-room exits", () => {
    expect(
      badgeForBot({ status: "done", finishReason: "waiting-room:waiting-room" }),
    ).toEqual({
      label: "Done (waiting)",
      badgeKey: "done-waiting",
    });
  });

  it("returns 'Done (waiting)' for waiting-for-host exits", () => {
    expect(
      badgeForBot({ status: "done", finishReason: "waiting-room:waiting-for-host" }),
    ).toEqual({
      label: "Done (waiting)",
      badgeKey: "done-waiting",
    });
  });

  it("returns 'Failed' for status=failed regardless of finishReason", () => {
    expect(
      badgeForBot({ status: "failed", finishReason: "meeting-rejected:rejected" }),
    ).toEqual({
      label: "Failed",
      badgeKey: "failed",
    });
  });

  it("title-cases unknown statuses as a fallback", () => {
    expect(badgeForBot({ status: "totally-new-state" })).toEqual({
      label: "Totally New State",
      badgeKey: "totally-new-state",
    });
  });
});

describe("STATUS_BADGE_CLASS coverage", () => {
  it("has a Tailwind class for the new done-waiting badge", () => {
    expect(STATUS_BADGE_CLASS["done-waiting"]).toMatch(/sky/);
  });

  it("retains classes for the existing status keys", () => {
    for (const key of ["launching", "joining", "in-meeting", "leaving", "done", "failed"]) {
      expect(STATUS_BADGE_CLASS[key]).toBeTruthy();
    }
  });
});
