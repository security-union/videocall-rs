import { test, expect, chromium } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

test("debug toast visibility", async ({ baseURL }) => {
  const uiURL = baseURL || "http://localhost:3001";
  const meetingId = `dbg_toast_${Date.now()}`;

  const browser1 = await chromium.launch({ args: BROWSER_ARGS });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS });

  try {
    // Create authenticated contexts
    const hostCtx = await browser1.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
    const token1 = generateSessionToken("host-dbg@test.rs", "DebugHost");
    const url = new URL(uiURL);
    await hostCtx.addCookies([
      {
        name: COOKIE_NAME,
        value: token1,
        domain: url.hostname,
        path: "/",
        httpOnly: true,
        secure: false,
        sameSite: "Lax",
      },
    ]);

    const guestCtx = await browser2.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
    const token2 = generateSessionToken("guest-dbg@test.rs", "DebugGuest");
    await guestCtx.addCookies([
      {
        name: COOKIE_NAME,
        value: token2,
        domain: url.hostname,
        path: "/",
        httpOnly: true,
        secure: false,
        sameSite: "Lax",
      },
    ]);

    const hostPage = await hostCtx.newPage();
    const guestPage = await guestCtx.newPage();

    // Capture console messages
    const hostConsole: string[] = [];
    hostPage.on("console", (msg) => hostConsole.push(`[${msg.type()}] ${msg.text()}`));

    // Host navigates and joins
    await hostPage.goto("/");
    await hostPage.waitForTimeout(1500);
    await hostPage.locator("#meeting-id").click();
    await hostPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
    await hostPage.locator("#username").click();
    await hostPage.locator("#username").fill("");
    await hostPage.locator("#username").pressSequentially("DebugHost", { delay: 50 });
    await hostPage.waitForTimeout(500);
    await hostPage.locator("#username").press("Enter");
    await expect(hostPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
    await hostPage.waitForTimeout(1500);

    // Host clicks Start/Join Meeting
    const hostJoinBtn = hostPage.getByText(/Start Meeting|Join Meeting/);
    const hostWait = hostPage.getByText("Waiting to be admitted");
    const result = await Promise.race([
      hostJoinBtn.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      hostWait.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    ]);
    if (result === "join") {
      await hostPage.waitForTimeout(1000);
      await hostJoinBtn.click();
      await hostPage.waitForTimeout(3000);
    }
    await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Start polling for the toast BEFORE the guest joins so we catch it
    // even if PARTICIPANT_JOINED fires early.
    const toastLocator = hostPage.locator(".peer-toast");
    const toastPromise = expect(toastLocator.first()).toBeVisible({ timeout: 30_000 });

    // Guest navigates and joins
    await guestPage.goto("/");
    await guestPage.waitForTimeout(1500);
    await guestPage.locator("#meeting-id").click();
    await guestPage.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
    await guestPage.locator("#username").click();
    await guestPage.locator("#username").fill("");
    await guestPage.locator("#username").pressSequentially("DebugGuest", { delay: 50 });
    await guestPage.waitForTimeout(500);
    await guestPage.locator("#username").press("Enter");
    await expect(guestPage).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
    await guestPage.waitForTimeout(1500);

    // Guest clicks Start/Join Meeting (handle waiting room)
    const guestJoinBtn = guestPage.getByText(/Start Meeting|Join Meeting/);
    const guestWait = guestPage.getByText("Waiting to be admitted");
    const guestResult = await Promise.race([
      guestJoinBtn.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      guestWait.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
    ]);
    if (guestResult === "waiting") {
      // Admit from host side
      const admitBtn = hostPage.getByTitle("Admit").first();
      await expect(admitBtn).toBeVisible({ timeout: 20_000 });
      await admitBtn.dispatchEvent("click");
      await hostPage.waitForTimeout(3000);
      const guestJoinPost = guestPage.getByText(/Join Meeting|Start Meeting/);
      const guestGrid = guestPage.locator("#grid-container");
      const post = await Promise.race([
        guestJoinPost.waitFor({ timeout: 20_000 }).then(() => "join" as const),
        guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
      ]);
      if (post === "join") {
        await guestPage.waitForTimeout(1000);
        await guestJoinPost.click();
        await guestPage.waitForTimeout(3000);
      }
    } else {
      await guestPage.waitForTimeout(1000);
      await guestJoinBtn.click();
      await guestPage.waitForTimeout(3000);
    }

    await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // Wait for the toast (polling started before guest joined)
    console.log("Both users in meeting. Waiting for PARTICIPANT_JOINED toast...");
    await toastPromise;

    // Check for ANY .peer-toast or .peer-toasts elements
    const toastContainer = await hostPage.locator(".peer-toasts").count();
    const toasts = await hostPage.locator(".peer-toast").count();
    console.log(`Host page: .peer-toasts count=${toastContainer}, .peer-toast count=${toasts}`);

    // Dump relevant console logs
    const participantLogs = hostConsole.filter(
      (l) =>
        l.includes("PARTICIPANT") ||
        l.includes("peer_joined") ||
        l.includes("should_emit") ||
        l.includes("MeetingEvent") ||
        l.includes("TOAST-RX") ||
        l.includes("Peer joined") ||
        l.includes("Peer left"),
    );
    console.log(`Host console logs with PARTICIPANT/toast/peer (${participantLogs.length}):`);
    participantLogs.forEach((l) => console.log(`  ${l}`));

    // Show ALL error messages
    const errorLogs = hostConsole.filter(
      (l) => l.startsWith("[error]") || l.includes("panic") || l.includes("Error"),
    );
    console.log(`Host error messages (${errorLogs.length}):`);
    errorLogs.forEach((l) => console.log(`  ${l}`));

    // Also dump ALL info-level logs
    const infoLogs = hostConsole.filter((l) => l.startsWith("[info]") || l.startsWith("[log]"));
    console.log(`Host info/log messages (${infoLogs.length}):`);
    infoLogs.slice(-30).forEach((l) => console.log(`  ${l}`));

    // Check page HTML for any toast elements
    const html = await hostPage.locator("body").innerHTML();
    const hasPeerToast = html.includes("peer-toast");
    console.log(`HTML contains 'peer-toast': ${hasPeerToast}`);

    expect(toasts).toBeGreaterThan(0);
  } finally {
    await browser1.close();
    await browser2.close();
  }
});
