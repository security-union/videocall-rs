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
    // form-login is a valid explicit override too.
    expect(chooseAuthBackend("app.videocall.labsworkspace.fnxlabs.com", "form-login")).toBe(
      "form-login",
    );
  });

  // PR #2082 blocker regression: form-login must NEVER be auto-selected.
  // An earlier revision auto-selected form-login for any non-JWT host when
  // BOT_EMAIL/BOT_PASSWORD were present, which could type real creds into a
  // third-party login form (e.g. Google on app.videocall.rs). The only path
  // to form-login now is an explicit override.
  it("never auto-selects form-login — non-JWT hosts default to storage-state", () => {
    // The reference form-login target itself still defaults to storage-state
    // with no override. This is the mutation guard: re-introducing any
    // host-based (or env-based) form-login auto-select flips one of these to
    // "form-login" and fails.
    expect(chooseAuthBackend("app.videocall.labsworkspace.fnxlabs.com")).toBe("storage-state");
    expect(chooseAuthBackend("app.videocall.rs")).toBe("storage-state");
    expect(chooseAuthBackend("evil.example.com")).toBe("storage-state");
  });

  it("only reaches form-login via an explicit override", () => {
    // Explicit opt-in is the sole route — for the reference host AND for a
    // host that would otherwise resolve to jwt or storage-state.
    expect(chooseAuthBackend("app.videocall.labsworkspace.fnxlabs.com", "form-login")).toBe(
      "form-login",
    );
    expect(chooseAuthBackend("app.videocall.rs", "form-login")).toBe("form-login");
    // An explicit override always wins over the host-based auto-selection.
    expect(chooseAuthBackend("app.videocall.labsworkspace.fnxlabs.com", "none")).toBe("none");
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
