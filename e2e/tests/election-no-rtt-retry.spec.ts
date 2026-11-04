import { test, expect, chromium } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { ELECTION_STARVED_GRACE_MS } from "../helpers/rust-mirrored-constants";
import { setRuntimeConfig } from "../helpers/runtime-config";

/**
 * The relay echoes an RTT probe back byte-identical (`ws_chat_session.rs`
 * writes back the bytes it was handed, with no relay-side stamping), and
 * WebSocket has no datagram concept so the probe travels as one binary frame.
 * WebTransport is switched off so the starved WebSocket candidate is the only
 * one; with it on, a WebSocket-preferred join falls back to WebTransport.
 * Untagged on purpose: this drives a ~15s famine and is not smoke-test material,
 * so it runs under `--project=dioxus` only, never in the per-PR `@bvt` sweep.
 */

const UI_URL = "http://localhost:3001";

const TERMINAL_FAILURE_LINE = /Election decision:.*outcome=failed/;
const ELECTION_START_LINE = /Starting connection election: preferred=ws preferred_timeout_ms=(\d+)/;
const LIVE_BUT_STARVED_LINE = /Election candidate:.*status=connected rtt_samples=0/;

const MARGIN_MS = 1_000;

test("a WebSocket-only election with no RTT samples waits out its deadline and grace, then fails", async () => {
  test.setTimeout(120_000);

  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(
    browser,
    "election-no-rtt@videocall.rs",
    "NoRttUser",
    UI_URL,
  );
  // The RTT famine below intercepts ws://localhost:8080 only.
  await setRuntimeConfig(context, { webTransportEnabled: "false" });
  await pinWebSocketTransport(context);
  const page = await context.newPage();

  try {
    const sentFrames = new Set<string>();
    let echoesDropped = 0;
    const key = (frame: string | Buffer) =>
      typeof frame === "string" ? `s:${frame}` : `b:${frame.toString("hex")}`;

    await page.routeWebSocket(/localhost:8080/, (ws) => {
      const server = ws.connectToServer();
      ws.onMessage((frame) => {
        sentFrames.add(key(frame));
        server.send(frame);
      });
      server.onMessage((frame) => {
        if (sentFrames.has(key(frame))) {
          echoesDropped++;
          return;
        }
        ws.send(frame);
      });
    });

    const log: Array<{ at: number; text: string }> = [];
    let electionStartedAt = 0;
    let preferredTimeoutMs = 0;
    page.on("console", (msg) => {
      const text = msg.text();
      const at = Date.now();
      const start = text.match(ELECTION_START_LINE);
      if (!electionStartedAt && start) {
        electionStartedAt = at;
        preferredTimeoutMs = Number(start[1]);
      }
      log.push({ at, text });
    });

    const meetingId = `e2e_no_rtt_${Date.now()}`;
    await page.goto(`${UI_URL}/`);
    await page.locator("#meeting-id").waitFor({ timeout: 30_000 });
    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 30 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("NoRttUser", { delay: 30 });
    await page.locator("#username").press("Enter");
    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    const grid = page.locator("#grid-container");
    const reached = await Promise.race([
      joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
    ]);
    if (reached === "join") {
      await joinButton
        .first()
        .click({ timeout: 15_000 })
        .catch(() => {
          // The auto-join effect may have unmounted the button first.
        });
    }
    await expect(grid).toBeVisible({ timeout: 20_000 });

    await page.waitForTimeout(15_000);

    expect(echoesDropped, "the proxy must have swallowed RTT echoes").toBeGreaterThan(0);
    expect(electionStartedAt, "the client must have started an election").toBeGreaterThan(0);

    const texts = log.map((l) => l.text);

    expect(
      texts.filter((t) => LIVE_BUT_STARVED_LINE.test(t)),
      "the election must have seen a CONNECTED candidate with zero RTT samples",
    ).not.toHaveLength(0);

    const firstFailure = log.find((l) => TERMINAL_FAILURE_LINE.test(l.text));
    expect(
      firstFailure,
      "the famine is permanent, so the election must eventually fail",
    ).toBeTruthy();
    expect(firstFailure!.text).toContain("preferred=ws phase=preferred");
    expect(firstFailure!.text).toContain("transport_pick=none");
    const graceEndMs = preferredTimeoutMs + ELECTION_STARVED_GRACE_MS;
    const waitedMs = firstFailure!.at - electionStartedAt;
    expect(waitedMs, "a starved candidate is waited for through the grace").toBeGreaterThanOrEqual(
      graceEndMs - 250,
    );
    expect(waitedMs, "and not beyond it").toBeLessThan(graceEndMs + MARGIN_MS);

    await expect(grid).toBeVisible();
    await expect(page.locator(".connection-led").first()).toHaveClass(/connecting/);
  } finally {
    await context.close();
    await browser.close();
  }
});
