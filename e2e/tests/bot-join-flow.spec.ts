// Real-DOM SMOKE coverage of the bots-app join flow against the live UI:
// operator-started host (prompt-fill -> Join attempt -> owner Start), idempotent
// auto-join re-entry, and a second authenticated peer joining. It exercises the
// production `joinMeetingAndEnableMedia` state machine end-to-end.
//
// NOTE: this is NOT the #865 regression lock. Reverting the #865 gate
// reintroduces a Promise.race whose prompt arm still usually wins, so this spec
// false-passes on the un-fixed code (verified by mutation). The deterministic,
// mutation-sensitive #865 lock lives in bots-app/src/meeting-join.test.ts
// (`waitForJoinButton — #865 form-gating`), which runs in per-PR CI via
// `npm run test:unit`. This spec is intentionally UNtagged (no @bvt) — it is
// validated on demand via `make e2e SPEC=bot-join-flow`, not gated per-PR, to
// keep a 2-context + media spec out of the smoke gate.
import { expect, test, type BrowserContext, type Page } from "@playwright/test";

import { joinMeetingAndEnableMedia } from "../bots-app/src/meeting-join";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

type JoinEvent = `input:${string}` | `click:${"Start Meeting" | "Join Meeting"}`;

async function installJoinEventRecorder(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const events: string[] = [];
    Object.defineProperty(window, "__botJoinEvents", {
      configurable: true,
      value: events,
    });
    document.addEventListener(
      "input",
      (event) => {
        const target = event.target;
        if (
          target instanceof HTMLInputElement &&
          target.placeholder === "Enter your display name"
        ) {
          events.push(`input:${target.value}`);
        }
      },
      true,
    );
    document.addEventListener(
      "click",
      (event) => {
        const target = event.target;
        const button = target instanceof Element ? target.closest("button") : null;
        const label = button?.textContent?.trim();
        if (label === "Start Meeting" || label === "Join Meeting") {
          events.push(`click:${label}`);
        }
      },
      true,
    );
  });
}

async function readJoinEvents(page: Page): Promise<JoinEvent[]> {
  return (await page.evaluate(
    () => (window as Window & { __botJoinEvents?: string[] }).__botJoinEvents ?? [],
  )) as JoinEvent[];
}

async function createAuthenticatedPeer(
  browserContext: BrowserContext,
  baseURL: string,
  identity: { email: string; name: string },
): Promise<Page> {
  await injectSessionCookie(browserContext, {
    baseURL,
    email: identity.email,
    name: identity.name,
  });
  await browserContext.addInitScript((displayName: string) => {
    localStorage.setItem("vc_display_name", displayName);
  }, identity.name);
  return await browserContext.newPage();
}

test.describe("bots-app real pre-join flow", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("prompt fill wins before Join, then handles Start, auto-join, and Join", async ({
    page,
    context,
    browser,
    baseURL,
  }) => {
    const uiURL = baseURL ?? "http://localhost:3001";
    const meetingId = `e2e_bot_join_${Date.now()}`;
    const promptName = "BotPromptHost";

    await injectSessionCookie(context, {
      baseURL: uiURL,
      email: "bot-prompt-host@videocall.rs",
      name: promptName,
    });
    await installJoinEventRecorder(page);
    await page.goto(`/meeting/${meetingId}`);

    const displayNameInput = page.locator('input[placeholder="Enter your display name"]');
    await expect(displayNameInput).toBeVisible({ timeout: 15_000 });

    await joinMeetingAndEnableMedia({
      page,
      participant: "prompt-host",
      displayName: promptName,
      meetingId,
    });
    await expect(page.locator("#grid-container")).toBeVisible();

    const hostEvents = await readJoinEvents(page);
    const completedInput = hostEvents.indexOf(`input:${promptName}`);
    const promptSubmit = hostEvents.indexOf("click:Join Meeting");
    expect(completedInput).toBeGreaterThanOrEqual(0);
    expect(promptSubmit).toBeGreaterThan(completedInput);
    expect(hostEvents).toContain("click:Start Meeting");

    const eventCountBeforeAutoJoin = hostEvents.length;
    await joinMeetingAndEnableMedia({
      page,
      participant: "already-joined-host",
      displayName: promptName,
      meetingId,
    });
    await expect(page.locator("#grid-container")).toBeVisible();
    expect(await readJoinEvents(page)).toHaveLength(eventCountBeforeAutoJoin);

    const peerContext = await browser.newContext({
      baseURL: uiURL,
      ignoreHTTPSErrors: true,
    });
    try {
      const peerName = "BotJoinPeer";
      const peerPage = await createAuthenticatedPeer(peerContext, uiURL, {
        email: "bot-join-peer@videocall.rs",
        name: peerName,
      });
      await peerPage.goto(`/meeting/${meetingId}`);

      await expect(peerPage.getByRole("button", { name: "Join Meeting" })).toBeVisible({
        timeout: 20_000,
      });
      await joinMeetingAndEnableMedia({
        page: peerPage,
        participant: "join-peer",
        displayName: peerName,
        meetingId,
      });
      await expect(peerPage.locator("#grid-container")).toBeVisible();
    } finally {
      await peerContext.close();
    }
  });
});
