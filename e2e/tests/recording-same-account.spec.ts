/**
 * Regression guard for the per-recorder recording-icon "same account, two
 * sessions" bug (production report). `RecordingSetCtx` used to be keyed by
 * `user_id` (= JWT `sub` = email); two tabs sharing one account therefore
 * shared a single recording bit, so a non-recording sibling tab falsely
 * showed the indicator. Fixed by re-keying on `session_id`.
 *
 * Shape:
 *   - Alena  : distinct account (host), records.
 *   - Alisa  : account SHARED@..., records.
 *   - Viktor : SAME account SHARED@... (sibling session, different display name),
 *              never touches the record button.
 *
 * This spec asserts the session-keying fix ONLY (see the "Fix A regression
 * guard" comment below for what is and isn't in scope — delivering a
 * recorder's re-announce to a sibling session of the same account, "Fix B",
 * is a separate, deferred change and is NOT asserted here).
 *
 * This spec is intentionally verbose: it dumps every page's console so we can
 * see the actual PEER_EVENT recording_started receipts on the wire, and it
 * inspects the rendered `.recording-indicator` per peer-list row.
 */
import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

const STUB_FILE_PICKER_SCRIPT = `
  window.showSaveFilePicker = async function () {
    return { createWritable: async function () {
      return { write: async function () {}, close: async function () {}, abort: async function () {} };
    } };
  };
`;

async function ctx(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  await context.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await context.addInitScript(STUB_FILE_PICKER_SCRIPT);
  return context;
}

function wireConsole(page: Page, label: string) {
  page.on("console", (msg) => {
    const t = msg.text();
    if (
      t.includes("PEER_EVENT") ||
      t.includes("recording_started") ||
      t.includes("recording_stopped") ||
      t.includes("peer joined") ||
      t.includes("Dropping PEER_EVENT")
    ) {
      console.log(`[${label}] ${t}`);
    }
  });
}

async function joinMeetingFromPage(page: Page): Promise<"in-meeting" | "waiting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");
  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);
  if (result === "waiting") return "waiting";
  if (result === "auto-joined") return "in-meeting";
  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(2000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitIfNeeded(hostPage: Page, joinResult: string, joinerPage: Page) {
  if (joinResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(500);
    await admitButton.dispatchEvent("click");
    await joinerPage.locator("#grid-container").waitFor({ timeout: 20_000 });
  }
}

async function openPeerListSidebar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.mouse.move(400, 400);
  await page.waitForTimeout(300);
  const openPeersBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
  await openPeersBtn.click();
  await expect(page.locator("#peer-list-container.visible")).toBeVisible({ timeout: 10_000 });
}

// Report, for each peer-list row, its name and whether it renders the recording icon.
async function peerListRecordingState(page: Page): Promise<Array<{ name: string; rec: boolean }>> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll("#peer-list-container .peer_item"));
    return rows.map((row) => {
      const nameEl = row.querySelector(".peer_item_name_container");
      const name = (nameEl?.textContent || "").replace(/\s+/g, " ").trim();
      const rec = !!row.querySelector(".recording-indicator");
      return { name, rec };
    });
  });
}

test.describe("Same-account recording icon", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("shared-account sibling gets a false recording icon", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const apiURL = process.env.API_BASE_URL || "http://localhost:8081";
    const meetingId = `e2e_rec_sameacct_${Date.now()}`;
    const SHARED = `shared-rec-${Date.now()}@videocall.rs`;
    const hostEmail = "alena-host@videocall.rs";

    const bAlena = await chromium.launch({ args: BROWSER_ARGS });
    const bAlisa = await chromium.launch({ args: BROWSER_ARGS });
    const bViktor = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Create the meeting with recording_allowed_for_all=true so Alisa (an
      // authenticated non-host guest) also gets a record button — mirrors the
      // "two concurrent recorders" test in recording.spec.ts.
      const hostToken = generateSessionToken(hostEmail, "Alena");
      const createResp = await fetch(`${apiURL}/api/v1/meetings`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Cookie: `${COOKIE_NAME}=${hostToken}`,
        },
        body: JSON.stringify({
          meeting_id: meetingId,
          attendees: [],
          recording_allowed_for_all: true,
        }),
      });
      if (!createResp.ok) {
        throw new Error(
          `POST /api/v1/meetings failed (${createResp.status}): ${await createResp.text()}`,
        );
      }

      const alenaCtx = await ctx(bAlena, hostEmail, "Alena", uiURL);
      const alisaCtx = await ctx(bAlisa, SHARED, "Alisa", uiURL);
      const viktorCtx = await ctx(bViktor, SHARED, "Viktor", uiURL);

      const alena = await alenaCtx.newPage();
      const alisa = await alisaCtx.newPage();
      const viktor = await viktorCtx.newPage();
      wireConsole(alena, "ALENA");
      wireConsole(alisa, "ALISA");
      wireConsole(viktor, "VIKTOR");

      // ── Alena creates meeting, records ──────────────────────────────────
      await fillAndSubmitJoinForm(alena, meetingId, "Alena");
      await alena.waitForTimeout(1500);
      expect(await joinMeetingFromPage(alena)).toBe("in-meeting");
      await expect(alena.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // ── Alisa joins (shared account), records ───────────────────────────
      await fillAndSubmitJoinForm(alisa, meetingId, "Alisa");
      await alisa.waitForTimeout(1500);
      const alisaJoin = await joinMeetingFromPage(alisa);
      await admitIfNeeded(alena, alisaJoin, alisa);
      await expect(alisa.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Wait for Alena to actually see a remote tile before recording.
      await expect(alena.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Alena starts recording.
      const alenaRec = alena.getByTestId("record-button");
      await expect(alenaRec).toBeVisible({ timeout: 10_000 });
      await alenaRec.click();
      await expect(alena.locator(".recording-status-banner .toast-name")).toHaveText(/Recording/, {
        timeout: 15_000,
      });

      // Alisa starts recording.
      const alisaRec = alisa.getByTestId("record-button");
      await expect(alisaRec).toBeVisible({ timeout: 10_000 });
      await alisaRec.click();
      await expect(alisa.locator(".recording-status-banner .toast-name")).toHaveText(/Recording/, {
        timeout: 15_000,
      });

      // ── Viktor joins LATE (same account as Alisa), never records ────────
      await fillAndSubmitJoinForm(viktor, meetingId, "Viktor");
      await viktor.waitForTimeout(1500);
      const viktorJoin = await joinMeetingFromPage(viktor);
      await admitIfNeeded(alena, viktorJoin, viktor);
      await expect(viktor.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

      // Let re-announce peer events settle.
      await viktor.waitForTimeout(4000);

      // ── Inspect rendered recording icons via the peer list ──────────────
      await openPeerListSidebar(alena);
      await openPeerListSidebar(viktor);
      await alena.waitForTimeout(500);
      await viktor.waitForTimeout(500);

      const alenaRows = await peerListRecordingState(alena);
      const viktorRows = await peerListRecordingState(viktor);

      console.log("ALENA peer-list rows:", JSON.stringify(alenaRows));
      console.log("VIKTOR peer-list rows:", JSON.stringify(viktorRows));

      // ── Fix A regression guard ──────────────────────────────────────────
      // Alisa and Viktor are two tabs of ONE account (shared user_id). Alisa
      // records; Viktor never touches the record button. Because the per-recorder
      // set is now keyed by session_id (not the shared user_id), Viktor's row must
      // NOT inherit Alisa's recording icon.
      //
      // We assert this STRUCTURALLY (row count + recording-bit pattern) rather
      // than by matching row text against "Alisa"/"Viktor": both sibling rows
      // render with the SAME display-name text on Alena's view (a separate,
      // pre-existing bug in per-session display-name resolution for two sessions
      // sharing one user_id — unrelated to recording, out of scope here, and not
      // introduced by this change). The recording bit itself is correctly
      // per-session regardless of that label bug: exactly one of the two sibling
      // rows must carry the icon (the one that is genuinely Alisa's recording
      // session), and exactly one must not (Viktor's).
      //
      // Pre-fix (user_id keying): BOTH sibling rows would show 🔴, because they
      // resolve to the same user_id as the recording sibling. Reverting Fix A
      // makes both `rec: true` and the "exactly one" assertion fails.
      const siblingRows = alenaRows.filter((r) => !r.name.includes("Host"));
      expect(siblingRows, "both of Alisa's and Viktor's rows must be present").toHaveLength(2);
      const recordingSiblings = siblingRows.filter((r) => r.rec);
      const nonRecordingSiblings = siblingRows.filter((r) => !r.rec);
      expect(
        recordingSiblings,
        "exactly one sibling session (Alisa's) must show the recording icon",
      ).toHaveLength(1);
      expect(
        nonRecordingSiblings,
        "exactly one sibling session (Viktor's) must NOT show a false recording icon",
      ).toHaveLength(1);

      // NOTE: We deliberately do NOT assert that Alisa's icon reaches Viktor's own
      // view. Delivering a recorder's re-announce to a sibling session of the same
      // user_id is the separately-tracked Fix B (the on_peer_joined self-guard is
      // user_id-scoped) and is explicitly out of scope for this change.
    } finally {
      await bAlena.close();
      await bAlisa.close();
      await bViktor.close();
    }
  });
});
