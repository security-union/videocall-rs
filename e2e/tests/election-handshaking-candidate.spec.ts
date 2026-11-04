import { test, expect } from "@playwright/test";
import { createAuthenticatedContext, pinWebSocketTransport } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { ELECTION_STARVED_GRACE_MS } from "../helpers/rust-mirrored-constants";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { clickJoinAndEnterGrid } from "../helpers/screen-share-meeting";

// Untagged: it holds a join's WebSocket upgrade for several seconds.
const START_LINE = /Starting connection election: preferred=ws preferred_timeout_ms=(\d+)/;

test("a WebSocket-only join waits for a candidate still handshaking past its connect timeout", async ({
  browser,
  baseURL,
}) => {
  test.setTimeout(120_000);
  const tag = `${Date.now()}`;
  const context = await createAuthenticatedContext(
    browser,
    `handshaking-${tag}@videocall.rs`,
    "Handshaking",
    baseURL!,
  );
  // WebTransport off, or the expired WebSocket phase would fall back to it.
  await setRuntimeConfig(context, { webTransportEnabled: "false" });
  await pinWebSocketTransport(context);
  const page = await context.newPage();
  const lines: string[] = [];
  let startedAt = 0;
  let timeoutMs = 0;
  page.on("console", (msg) => {
    const start = msg.text().match(START_LINE);
    if (start && !startedAt) {
      startedAt = Date.now();
      timeoutMs = Number(start[1]);
    }
    lines.push(msg.text());
  });
  // Awaiting before connectToServer keeps the page socket CONNECTING.
  let held = false;
  await page.routeWebSocket(/localhost:8080/, async (ws) => {
    if (!held) {
      held = true;
      await expect.poll(() => startedAt, { timeout: 10_000 }).toBeGreaterThan(0);
      await page.waitForTimeout(startedAt + timeoutMs + 1_000 - Date.now());
    }
    ws.connectToServer();
  });

  try {
    await fillAndSubmitJoinForm(page, `e2e_handshaking_${tag}`, "Handshaking");
    await clickJoinAndEnterGrid(page);
    const firstDecision = () => lines.find((l) => l.includes("Election decision:"));
    await expect.poll(firstDecision, { timeout: 30_000 }).toBeTruthy();
    const decision = firstDecision()!;
    expect(decision).toContain("outcome=elected");
    expect(decision).toContain("preferred=ws phase=preferred");
    expect(decision).toContain(" elected=ws_0 ");
    const durationMs = Number(decision.match(/election_duration_ms=(\d+)/)?.[1]);
    expect(durationMs).toBeGreaterThan(timeoutMs);
    expect(durationMs).toBeLessThan(timeoutMs + ELECTION_STARVED_GRACE_MS);
  } finally {
    await context.close();
  }
});
