import { test, expect, chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";

/**
 * The relay echoes an RTT probe back byte-identical (`ws_chat_session.rs`
 * writes back the bytes it was handed, with no relay-side stamping), and
 * WebSocket has no datagram concept so the probe travels as one binary frame.
 * Untagged on purpose: this drives a ~10s famine and is not smoke-test material,
 * so it runs under `--project=dioxus` only, never in the per-PR `@bvt` sweep.
 */

const UI_URL = "http://localhost:3001";

const RETRY_LINE = /retrying the scan in \d+ms \(retry (\d)\/3\)/;
// The TERMINAL give-up: `error!("Election failed: …")` also fires on every retried
// round. Pinned by `only_the_terminal_round_emits_an_election_decision`.
const TERMINAL_FAILURE_LINE = /Election decision:.*outcome=failed/;
const ELECTION_START_LINE = /Starting connection election for/;
const LIVE_BUT_STARVED_LINE = /Election candidate:.*is_connected=true rtt_samples=0/;

// Above the un-fixed ceiling (base + every extension), below the fixed one (+3 retry rounds).
const MIN_MS_BEFORE_GIVING_UP = 6_000;

test("election with no RTT samples keeps testing instead of failing the join", async () => {
  test.setTimeout(120_000);

  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(
    browser,
    "election-no-rtt@videocall.rs",
    "NoRttUser",
    UI_URL,
  );
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
    page.on("console", (msg) => {
      const text = msg.text();
      const at = Date.now();
      if (!electionStartedAt && ELECTION_START_LINE.test(text)) {
        electionStartedAt = at;
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

    const retryRounds = texts
      .map((t) => t.match(RETRY_LINE))
      .filter((m): m is RegExpMatchArray => m !== null)
      .map((m) => m[1]);
    expect(
      retryRounds.slice(0, 3),
      "a measurement-less election with a live candidate must be retried 3x",
    ).toEqual(["1", "2", "3"]);

    const firstFailure = log.find((l) => TERMINAL_FAILURE_LINE.test(l.text));
    expect(
      firstFailure,
      "the famine is permanent, so the election must eventually fail",
    ).toBeTruthy();
    expect(
      firstFailure!.at - electionStartedAt,
      "the client must keep testing for the full retry budget before giving up",
    ).toBeGreaterThanOrEqual(MIN_MS_BEFORE_GIVING_UP);

    await expect(grid).toBeVisible();
    await expect(page.locator(".connection-led").first()).toHaveClass(/connecting/);
  } finally {
    await context.close();
    await browser.close();
  }
});
