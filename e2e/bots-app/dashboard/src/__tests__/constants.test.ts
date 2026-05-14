import { describe, expect, it } from "vitest";

import { badgeForBot, STATUS_BADGE_CLASS } from "../lib/constants";

describe("badgeForBot status mapping", () => {
  it("returns the raw status label for in-progress bots", () => {
    expect(badgeForBot({ status: "in-meeting" })).toEqual({
      label: "in-meeting",
      badgeKey: "in-meeting",
    });
  });

  it("returns 'done' for bots that exited via ttl-expired", () => {
    expect(badgeForBot({ status: "done", finishReason: "ttl-expired" })).toEqual({
      label: "done",
      badgeKey: "done",
    });
  });

  it("returns 'done (waiting)' for waiting-room exits", () => {
    expect(
      badgeForBot({ status: "done", finishReason: "waiting-room:waiting-room" }),
    ).toEqual({
      label: "done (waiting)",
      badgeKey: "done-waiting",
    });
  });

  it("returns 'done (waiting)' for waiting-for-host exits", () => {
    expect(
      badgeForBot({ status: "done", finishReason: "waiting-room:waiting-for-host" }),
    ).toEqual({
      label: "done (waiting)",
      badgeKey: "done-waiting",
    });
  });

  it("returns 'failed' for status=failed regardless of finishReason", () => {
    expect(
      badgeForBot({ status: "failed", finishReason: "meeting-rejected:rejected" }),
    ).toEqual({
      label: "failed",
      badgeKey: "failed",
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
