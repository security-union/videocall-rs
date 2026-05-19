import { describe, expect, it } from "vitest";

import {
  isValidMeetingUrl,
  isValidParticipant,
  validateLaunchForm,
  type LaunchFormValues,
} from "../lib/validation";

const baseValues: LaunchFormValues = {
  meetingURL: "https://app.videocall.fnxlabs.com/meeting/TonyBots",
  participant: "alice",
  displayName: "",
  ttl: "5m",
  network: "none",
  headless: false,
  authBackend: "jwt",
  storageStateFile: "",
  runLocation: "local",
  costume: "default",
  audio: "default",
};

describe("isValidMeetingUrl", () => {
  it.each([
    "https://app.videocall.fnxlabs.com/meeting/X",
    "http://localhost:3001/meeting/abc",
  ])("accepts %s", (url) => {
    expect(isValidMeetingUrl(url)).toBe(true);
  });

  it.each([
    "",
    "not-a-url",
    "ftp://example.com/meeting/X",
    "https://example.com/no-path",
  ])("rejects %p", (url) => {
    expect(isValidMeetingUrl(url)).toBe(false);
  });
});

describe("isValidParticipant", () => {
  it("accepts handles + emails", () => {
    expect(isValidParticipant("alice")).toBe(true);
    expect(isValidParticipant("alice@example.com")).toBe(true);
    expect(isValidParticipant("bob.smith+test_1")).toBe(true);
  });

  it("rejects empty + whitespace", () => {
    expect(isValidParticipant("")).toBe(false);
    expect(isValidParticipant("   ")).toBe(false);
    expect(isValidParticipant("alice bob")).toBe(false);
  });
});

describe("validateLaunchForm", () => {
  it("passes when all fields are valid", () => {
    expect(validateLaunchForm(baseValues)).toEqual({});
  });

  it("flags an invalid meeting URL", () => {
    const r = validateLaunchForm({ ...baseValues, meetingURL: "nope" });
    expect(r.meetingURL).toBeDefined();
  });

  it("flags an invalid TTL", () => {
    const r = validateLaunchForm({ ...baseValues, ttl: "5x" });
    expect(r.ttl).toBeDefined();
  });

  it("flags an unknown netsim profile", () => {
    const r = validateLaunchForm({ ...baseValues, network: "bogus" });
    expect(r.network).toBeDefined();
  });

  it("requires storage-state-file when auth=storage-state", () => {
    const r = validateLaunchForm({ ...baseValues, authBackend: "storage-state" });
    expect(r.storageStateFile).toBeDefined();
  });

  it("rejects non-local run locations", () => {
    const r = validateLaunchForm({ ...baseValues, runLocation: "future-vm" });
    expect(r.runLocation).toBeDefined();
  });
});
