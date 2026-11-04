import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
  pinWebTransportTransport,
} from "../helpers/auth-context";
import { joinAndStartCamera, joinAsGuestAndStartCamera } from "../helpers/camera-publisher";
import { readKeyframeRequestFilteredTotal } from "../helpers/relay-metrics";
import { waitForServices } from "../helpers/wait-for-services";

const DEFAULT_UI_URL = "http://localhost:3001";

// Issue #3206: the relay delivers a keyframe request only to the participant it
// names. Three participants are needed: with two, the only other session is
// always the target, so nothing is ever filtered.

async function expectElected(
  p: { lines: string[] },
  transport: "ws" | "wt",
  message: string,
): Promise<void> {
  await expect
    .poll(() => p.lines.some((l) => l.includes(`Elected connection ${transport}_`)), {
      timeout: 30_000,
      message,
    })
    .toBe(true);
}

function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => lines.push(msg.text()));
  return lines;
}

async function participant(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
  transport: "websocket" | "webtransport",
): Promise<{ context: BrowserContext; page: Page; lines: string[] }> {
  const context = await createAuthenticatedContext(browser, email, name, uiURL);
  if (transport === "webtransport") {
    await pinWebTransportTransport(context);
  } else {
    await pinWebSocketTransport(context);
  }
  const page = await context.newPage();
  return { context, page, lines: collectConsole(page) };
}

test.describe("keyframe requests reach only their target (#3206)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a late joiner's requests reach each publisher across both relays, and the copies for others are dropped", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const stamp = Date.now();
    const meetingId = `e2e_3206_kf_target_${stamp}`;
    const alice = `kf-alice-${stamp}@videocall.rs`;
    const bob = `kf-bob-${stamp}@videocall.rs`;
    const carol = `kf-carol-${stamp}@videocall.rs`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const a = await participant(browser, alice, "KfAlice", uiURL, "websocket");
      const b = await participant(browser, bob, "KfBob", uiURL, "webtransport");
      const c = await participant(browser, carol, "KfCarol", uiURL, "websocket");

      await joinAndStartCamera(a.page, meetingId);
      await joinAsGuestAndStartCamera(a.page, b.page, meetingId);
      await expectElected(a, "ws", "Alice must elect WebSocket");
      await expectElected(
        b,
        "wt",
        "Bob must elect WebTransport, so Carol's requests cross from one relay to the other",
      );

      await joinAsGuestAndStartCamera(a.page, c.page, meetingId);
      await expectElected(c, "ws", "Carol must elect WebSocket");

      const sentTo = (target: string) =>
        c.lines.some((l) => l.includes(`Sending KEYFRAME_REQUEST to ${target} `));
      const receivedFromCarol = (lines: string[]) =>
        lines.some((l) => l.includes(`Received KEYFRAME_REQUEST from ${carol} `));

      await expect
        .poll(() => sentTo(alice) && sentTo(bob), {
          timeout: 60_000,
          message: "precondition: Carol never requested a keyframe from both Alice and Bob",
        })
        .toBe(true);
      await expect
        .poll(() => receivedFromCarol(a.lines), {
          timeout: 30_000,
          message: "Alice (WebSocket) never received Carol's keyframe request",
        })
        .toBe(true);
      await expect
        .poll(() => receivedFromCarol(b.lines), {
          timeout: 30_000,
          message: "Bob (WebTransport) never received Carol's keyframe request",
        })
        .toBe(true);

      for (const relay of ["websocket", "webtransport"] as const) {
        await expect
          .poll(() => readKeyframeRequestFilteredTotal(relay, meetingId), {
            timeout: 30_000,
            message: `the ${relay} relay counted no dropped copy`,
          })
          .toBeGreaterThan(0);
      }

      for (const [who, p, kept, other] of [
        ["Alice", a, "ws", "wt"],
        ["Bob", b, "wt", "ws"],
        ["Carol", c, "ws", "wt"],
      ] as const) {
        expect(
          p.lines.some((l) => l.includes(`Elected connection ${other}_`)),
          `${who} must stay on its pinned transport (${kept})`,
        ).toBe(false);
      }
    } finally {
      await browser.close();
    }
  });
});
