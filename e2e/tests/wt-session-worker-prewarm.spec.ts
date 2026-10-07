/**
 * The real join adopts the WT session Worker booted at page mount (`use_wt_session_worker_prewarm`);
 * observer clients never adopt or refill it. `WorkerSession::start` logs
 * `WT session worker start: spare=booted|booting|none|skipped` (`skipped` = observer).
 *
 * UNTAGGED: runs only under `--project=dioxus`, not in per-PR CI.
 */

import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import {
  createMeeting,
  fetchMeetingState,
  joinMeeting,
  patchMeetingSettings,
} from "../helpers/meeting-api";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

const START_LINE = /WT session worker start: spare=(\w+)/;
const ADOPTED = /^(booted|booting)$/;
const WT_WASM_URL = /\/wt_session_worker_bg(-[0-9a-f]+)?\.wasm(\?|$)/;
const SPARE_FAILED_LINE = "prewarmed WT session worker failed";
const ELECTION_ACTIVE = /Election decision:.* active=(\S+)/;

async function pinWebTransport(context: BrowserContext): Promise<void> {
  // The prewarm hook only boots a spare when a join would open a WT candidate.
  await context.addInitScript(`localStorage.setItem("vc_transport_preference", "webtransport");`);
  await context.addInitScript(`localStorage.setItem("vc_transport_sticky", "true");`);
}

function captureConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => lines.push(msg.text()));
  return lines;
}

function workerStarts(lines: string[]): string[] {
  return lines.flatMap((line) => {
    const m = START_LINE.exec(line);
    return m ? [m[1]] : [];
  });
}

function electedActive(lines: string[]): string | undefined {
  for (const line of lines) {
    const m = ELECTION_ACTIVE.exec(line);
    if (m) return m[1];
  }
  return undefined;
}

function countElectionDecisions(lines: string[]): number {
  return lines.filter((line) => line.includes("Election decision:")).length;
}

async function navigateToMeetingViaHome(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.waitForTimeout(1500);
  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
}

async function enterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const grid = page.locator("#grid-container");
  const result = await waitForVisibleState(
    [
      { name: "join-button", locator: joinButton },
      { name: "grid", locator: grid },
    ] as const,
    30_000,
  );
  if (result === "join-button") {
    // After admission the page can auto-advance past this button, so a click that never lands is fine.
    await joinButton.click({ timeout: 5_000 }).catch(() => undefined);
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

test.describe("WT session worker prewarm", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a direct join adopts the spare worker booted on page mount", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wt_prewarm_direct_${Date.now()}`;
    const email = "wt-prewarm-direct@videocall.rs";
    const name = "WtPrewarmDirect";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
      const context = await createAuthenticatedContext(browser, email, name, uiURL);
      await pinWebTransport(context);
      const page = await context.newPage();
      const lines = captureConsole(page);

      await navigateToMeetingViaHome(page, meetingId, name);
      await enterGrid(page);

      await expect
        .poll(() => workerStarts(lines).length, {
          timeout: 20_000,
          message: "the join must open a WT session worker at all",
        })
        .toBeGreaterThan(0);
      const starts = workerStarts(lines);
      console.log(`[wt-prewarm direct] starts=${starts.join(",")}`);
      expect(
        starts[0],
        "the join's first WT start must adopt the spare booted while the page was mounted",
      ).toMatch(ADOPTED);
    } finally {
      await browser.close();
    }
  });

  test("waiting-room observers skip the spare and the real join adopts the one booted on page mount", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wt_prewarm_wr_${Date.now()}`;
    const hostEmail = "wt-prewarm-host@videocall.rs";
    const hostName = "WtPrewarmHost";
    const email = "wt-prewarm-waiter@videocall.rs";
    const name = "WtPrewarmWaiter";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeeting(hostEmail, hostName, { meetingId, waitingRoomEnabled: true });
      expect((await joinMeeting(hostEmail, hostName, meetingId, hostName)).status).toBe("admitted");
      await expect
        .poll(() => fetchMeetingState(hostEmail, hostName, meetingId), { timeout: 20_000 })
        .toBe("active");

      const context = await createAuthenticatedContext(browser, email, name, uiURL);
      await pinWebTransport(context);
      const page = await context.newPage();
      const lines = captureConsole(page);

      await navigateToMeetingViaHome(page, meetingId, name);
      const waitingCard = page.locator('[data-testid="meeting-waiting-room"]');
      await expect(waitingCard).toBeVisible({ timeout: 30_000 });

      // The page can run more than one observer client here; every one must finish its election.
      await expect
        .poll(
          () => {
            const started = workerStarts(lines).length;
            return started > 0 && countElectionDecisions(lines) >= started;
          },
          {
            timeout: 30_000,
            message:
              "every observer client must log its start and election decision before the snapshot",
          },
        )
        .toBe(true);
      const observerStarts = workerStarts(lines);
      expect(observerStarts, "an observer must never adopt the spare").toEqual(
        observerStarts.map(() => "skipped"),
      );

      await patchMeetingSettings(hostEmail, hostName, meetingId, { waiting_room_enabled: false });
      await expect(waitingCard).toBeHidden({ timeout: 30_000 });
      await enterGrid(page);

      const joinStarts = () => workerStarts(lines).filter((s) => s !== "skipped");
      await expect
        .poll(() => joinStarts().length, {
          timeout: 20_000,
          message: "the real join must open its own WT session worker",
        })
        .toBeGreaterThan(0);
      console.log(
        `[wt-prewarm waiting-room] observer=${observerStarts.join(",")} all=${workerStarts(lines).join(",")}`,
      );
      expect(
        joinStarts()[0],
        "the real join adopts the spare booted while the page was mounted",
      ).toBe("booted");
    } finally {
      await browser.close();
    }
  });

  test("a spare whose wasm failed to load is not adopted, so the join still elects WebTransport", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_wt_prewarm_wasm_fail_${Date.now()}`;
    const email = "wt-prewarm-wasm-fail@videocall.rs";
    const name = "WtPrewarmWasmFail";

    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      await createMeeting(email, name, { meetingId, waitingRoomEnabled: false });
      const context = await createAuthenticatedContext(browser, email, name, uiURL);
      await pinWebTransport(context);
      let wasmFetches = 0;
      // Only the page-mount spare's wasm fetch fails; the join's fresh worker gets the real one.
      await context.route(WT_WASM_URL, (route) =>
        ++wasmFetches === 1 ? route.abort() : route.continue(),
      );
      const page = await context.newPage();
      const lines = captureConsole(page);

      await navigateToMeetingViaHome(page, meetingId, name);
      await expect
        .poll(() => wasmFetches, {
          timeout: 20_000,
          message: "the page-mount spare must request its wasm, or nothing was injected",
        })
        .toBeGreaterThan(0);
      // Hold the join until the spare reports the failure; on un-fixed code it never does.
      const spareReportedFailure = await expect
        .poll(() => lines.some((line) => line.includes(SPARE_FAILED_LINE)), { timeout: 10_000 })
        .toBe(true)
        .then(
          () => true,
          () => false,
        );
      const preJoinWasmFetches = wasmFetches;
      await enterGrid(page);

      await expect
        .poll(() => electedActive(lines), {
          timeout: 30_000,
          message: "the join must reach an election decision",
        })
        .toBeDefined();
      const starts = workerStarts(lines);
      console.log(
        `[wt-prewarm wasm-fail] starts=${starts.join(",")} active=${electedActive(lines)} ` +
          `spareFailed=${spareReportedFailure} wasmFetches=${wasmFetches}`,
      );
      expect(
        electedActive(lines),
        "a dead spare must not be adopted, so WebTransport still wins the election",
      ).toMatch(/^wt_/);
      expect(starts[0], "the join must discard the failed spare and boot a fresh worker").toBe(
        "none",
      );
      expect(spareReportedFailure, "the spare's wasm load failure must reach Worker.onerror").toBe(
        true,
      );
      expect(
        preJoinWasmFetches,
        "a second prewarm before the join would replace the failed spare and void this test",
      ).toBe(1);
      expect(wasmFetches, "the join's fresh worker must fetch the wasm again").toBeGreaterThan(
        preJoinWasmFetches,
      );
    } finally {
      await browser.close();
    }
  });
});
