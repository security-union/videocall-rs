import { describe, it, expect } from "vitest";

import { isDevServerNoise } from "./dev-noise";

describe("isDevServerNoise", () => {
  const devUrl = "http://localhost:3001/meeting/Foo";
  const altDevUrl = "http://127.0.0.1:3001/meeting/Foo";
  const prodUrl = "https://app.videocall.rs/meeting/Foo";

  it("matches the dev-server Unexpected token '<' error when on localhost:3001", () => {
    expect(isDevServerNoise("Unexpected token '<'", { pageUrl: devUrl })).toBe(true);
  });

  it("matches the dev-server Unexpected token '<' error when on 127.0.0.1:3001", () => {
    expect(isDevServerNoise("Unexpected token '<'", { pageUrl: altDevUrl })).toBe(true);
  });

  it("does NOT match Unexpected token '<' when on a production URL", () => {
    expect(isDevServerNoise("Unexpected token '<'", { pageUrl: prodUrl })).toBe(false);
  });

  it("does NOT match a longer Unexpected token '<' message that just contains the phrase", () => {
    // Exact-match guard: a real syntax error referencing line numbers
    // should still be surfaced. Only the literal Dioxus dev-server
    // message is suppressed.
    expect(isDevServerNoise("Unexpected token '<' at line 42", { pageUrl: devUrl })).toBe(false);
  });

  it("matches the _dioxus HMR websocket failure regardless of page URL", () => {
    const ws =
      "WebSocket connection to 'ws://localhost:3001/_dioxus?build_id=0' failed: " +
      "Error during WebSocket handshake";
    expect(isDevServerNoise(ws, { pageUrl: devUrl })).toBe(true);
    expect(isDevServerNoise(ws, { pageUrl: prodUrl })).toBe(true);
  });

  it("does NOT match unrelated console errors", () => {
    expect(isDevServerNoise("ReferenceError: foo is not defined", { pageUrl: devUrl })).toBe(false);
    expect(
      isDevServerNoise("Failed to load resource: 500 (Internal Server Error)", {
        pageUrl: devUrl,
      }),
    ).toBe(false);
  });

  it("does NOT match Unexpected token '<' when the page URL is malformed", () => {
    // Defensive: a garbage URL should not be treated as a dev host.
    expect(isDevServerNoise("Unexpected token '<'", { pageUrl: "not a url" })).toBe(false);
  });
});
