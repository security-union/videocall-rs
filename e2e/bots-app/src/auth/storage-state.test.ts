import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { chooseAuthBackend, requireStorageState, storageStatePath } from "./storage-state";

describe("chooseAuthBackend", () => {
  it("picks JWT for localhost and 127.0.0.1", () => {
    expect(chooseAuthBackend("localhost")).toBe("jwt");
    expect(chooseAuthBackend("127.0.0.1")).toBe("jwt");
  });

  it("picks JWT for HCL daily and ascend hostnames", () => {
    expect(chooseAuthBackend("app.videocall.fnxlabs.com")).toBe("jwt");
    expect(chooseAuthBackend("websocket.videocall.fnxlabs.com")).toBe("jwt");
    expect(chooseAuthBackend("app.videocall.conceptcar7.com")).toBe("jwt");
  });

  it("picks JWT for PR-preview hostnames", () => {
    expect(chooseAuthBackend("pr12.preview.videocall.fnxlabs.com")).toBe("jwt");
  });

  it("picks storage-state for app.videocall.rs (public OSS)", () => {
    expect(chooseAuthBackend("app.videocall.rs")).toBe("storage-state");
  });

  it("picks storage-state for any other unrecognized hostname", () => {
    expect(chooseAuthBackend("evil.example.com")).toBe("storage-state");
  });

  it("honors an explicit override regardless of hostname", () => {
    expect(chooseAuthBackend("app.videocall.rs", "jwt")).toBe("jwt");
    expect(chooseAuthBackend("localhost", "storage-state")).toBe("storage-state");
  });
});

describe("storageStatePath", () => {
  it("joins the run-dir with auth/<account>.json", () => {
    expect(storageStatePath("/tmp/run", "alice")).toBe("/tmp/run/auth/alice.json");
  });
});

describe("requireStorageState", () => {
  let runDir: string;

  beforeEach(() => {
    runDir = mkdtempSync(join(tmpdir(), "bots-app-auth-test-"));
  });

  afterEach(() => {
    rmSync(runDir, { recursive: true, force: true });
  });

  it("returns the path when the file exists", () => {
    const authDir = join(runDir, "auth");
    mkdirSync(authDir, { recursive: true });
    const file = join(authDir, "alice.json");
    writeFileSync(file, '{"cookies":[]}');
    expect(requireStorageState(file)).toBe(file);
  });

  it("throws with login guidance when the file is missing", () => {
    expect(() => requireStorageState(join(runDir, "auth/alice.json"))).toThrow(
      /bots-app login <account>/,
    );
  });
});
