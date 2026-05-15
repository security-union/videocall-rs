import { describe, expect, it } from "vitest";

import { badgeForBot, networkLabel, STATUS_BADGE_CLASS } from "../lib/constants";

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

describe("networkLabel display mapping", () => {
  it("renders the 'none' passthrough preset as 'as is'", () => {
    // The underlying value sent to the server stays "none" — this
    // is a display-only mapping. The dashboard surfaces the
    // passthrough/disable-netsim option as "as is" so operators
    // recognize it at a glance.
    expect(networkLabel("none")).toBe("as is");
  });

  it("returns every non-passthrough preset name unchanged", () => {
    expect(networkLabel("good_wifi")).toBe("good_wifi");
    expect(networkLabel("good_4g")).toBe("good_4g");
    expect(networkLabel("congested_wifi")).toBe("congested_wifi");
    expect(networkLabel("lossy_mobile")).toBe("lossy_mobile");
    expect(networkLabel("satellite")).toBe("satellite");
    expect(networkLabel("dialup")).toBe("dialup");
  });

  it("does not mangle an unknown preset name (future-proofing)", () => {
    // If the Rust crate grows a new preset and the constant list
    // lags, the helper must still render the raw name rather than
    // hiding the value behind a stale special-case map.
    expect(networkLabel("brand_new_5g")).toBe("brand_new_5g");
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
