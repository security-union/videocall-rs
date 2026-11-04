import {
  test,
  expect,
  chromium,
  Browser,
  BrowserContext,
  Locator,
  Page,
  Response,
} from "@playwright/test";
import {
  BROWSER_ARGS,
  CERT_HASH_INIT_SCRIPT,
  createAuthenticatedContext,
  PIN_WEBTRANSPORT_TRANSPORT_INIT_SCRIPT,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { openMeetingOptions, wakeActionBar } from "../helpers/controls";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { JOIN_TONE_SPY, joinChimeCount } from "../helpers/join-chime-tones";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";
import { AnnouncementOverride, createMeeting, patchMeetingSettings } from "../helpers/meeting-api";
import { expectSelfRecording } from "../helpers/recording-status";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import {
  enterMeetingAsHost,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

interface User {
  email: string;
  name: string;
}

interface Session {
  browser: Browser;
  context: BrowserContext;
  page: Page;
}

type Field = "join-message" | "join-sound" | "leave-message" | "leave-sound";
type Choice = AnnouncementOverride;
type ToastKind = "joined" | "left";

const WIRE_FIELD: Record<Field, string> = {
  "join-message": "entry_message_override",
  "join-sound": "entry_sound_override",
  "leave-message": "exit_message_override",
  "leave-sound": "exit_sound_override",
};

const CHOICES: readonly Choice[] = ["participant", "on", "off"];
const GROUP_NAME = "Join & leave notifications";
const JOIN_MESSAGE_SWITCH = "#entry-notifications-toggle";
const JOIN_SOUND_SWITCH = "#entry-sound-toggle";
const LEAVE_MESSAGE_SWITCH = "#exit-notifications-toggle";
const LEAVE_SOUND_SWITCH = "#exit-sound-toggle";
const ENTRY_NOTIFICATIONS_KEY = "vc_appearance_entry_notifications";
const ENTRY_SOUND_KEY = "vc_appearance_entry_sound";
const EXIT_NOTIFICATIONS_KEY = "vc_appearance_exit_notifications";
const ANNOUNCE_SETTLE_MS = 2_000;
const LOBBY_SOCKET = /\/lobby/;
const WEBSOCKET_ONLY = { webTransportEnabled: "false" };
const CONNECTED_LOG = "Connection state changed: Connected";
const RECONNECTING_LOG = "Connection state changed: Reconnecting";
const CONNECTION_ESTABLISHED_LOG = "DIOXUS-UI: Connection established";
const SETTINGS_PUSH_LOG = "Received MEETING_SETTINGS_UPDATED";
const SETTINGS_READ_FAILED_LOG = "Failed to refresh meeting settings: Server error (503)";
const WS_CONNECT_TIMEOUT_MS = 15_000;
const RECONNECT_HOLD_CAP_MS = 12_000;
const SELF_WT_BADGE = '.transport-badge[aria-label="Your connection transport: WebTransport"]';
const OVERRIDES_TABLE_TESTID = "meeting-options-announcements-table";
const OVERRIDE_PICK_SETTLE_MS = 400;
const SETTLE_TIMER_SLACK_MS = 50;
const SETTLED_SAVE_DEADLINE_MS = 2_000;
const NO_SAVE_WINDOW_MS = 1_000;
const SETTINGS_BACK_BUTTON = ".settings-back-btn";
const SAME_CARD_MARK = "data-e2e-same-card";
const PREJOIN_PERMISSION_PROMPT = '[data-testid="prejoin-permission-prompt"]';
const PREJOIN_CAMERA_OPTIONS = '[data-testid="prejoin-camera-select"] option';
const PATCH_START_RECORDER = `(() => {
  const starts = [];
  window.__vcPatchStarts = starts;
  const original = window.fetch;
  window.fetch = function (input, init) {
    const request = input instanceof Request ? input : null;
    const method = (init && init.method) || (request ? request.method : "GET");
    if (String(method).toUpperCase() === "PATCH") {
      const url = new URL(request ? request.url : String(input), location.href);
      starts.push({ path: url.pathname, at: performance.now() });
    }
    return original.apply(this, arguments);
  };
})();`;

interface OverrideChange {
  testId: string;
  at: number;
}

interface PatchStart {
  path: string;
  at: number;
}

type TimingWindow = Window & {
  __vcOverrideChanges?: OverrideChange[];
  __vcPatchStarts?: PatchStart[];
};

function ownPrefsOff(keys: string[]): string {
  return keys.map((key) => `localStorage.setItem(${JSON.stringify(key)}, "false");`).join("\n");
}

function storedDisplayName(name: string): string {
  return `localStorage.setItem("vc_display_name", ${JSON.stringify(name)});`;
}

function consoleLines(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => lines.push(msg.text()));
  return lines;
}

function linesWith(lines: string[], needle: string): number {
  return lines.filter((line) => line.includes(needle)).length;
}

function statusReads(page: Page, meetingId: string): string[] {
  const reads: string[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "GET" &&
      new URL(request.url()).pathname === `/api/v1/meetings/${meetingId}/status`
    ) {
      reads.push(request.url());
    }
  });
  return reads;
}

interface StatusReadFault {
  arm: () => void;
  failed: () => number;
  reads: () => number;
}

async function failFirstArmedStatusRead(
  page: Page,
  meetingId: string,
  uiURL: string,
): Promise<StatusReadFault> {
  let armed = false;
  let failed = 0;
  let reads = 0;
  await page.route(
    (url) => url.pathname === `/api/v1/meetings/${meetingId}/status`,
    async (route) => {
      const request = route.request();
      if (!armed || request.method() !== "GET") {
        await route.fallback();
        return;
      }
      reads += 1;
      if (failed > 0) {
        await route.fallback();
        return;
      }
      failed += 1;
      await route.fulfill({
        status: 503,
        contentType: "application/json",
        headers: {
          "access-control-allow-origin":
            (await request.headerValue("origin")) ?? new URL(uiURL).origin,
          "access-control-allow-credentials": "true",
        },
        body: JSON.stringify({ success: false, error: "e2e forced failure" }),
      });
    },
  );
  return {
    arm: () => {
      armed = true;
    },
    failed: () => failed,
    reads: () => reads,
  };
}

interface SentSettingsPatch {
  body: unknown;
  afterRelease: boolean;
}

interface HeldSettingsPatch {
  release: () => void;
  sent: SentSettingsPatch[];
  answered: boolean[];
}

async function holdFirstSettingsPatch(page: Page, meetingId: string): Promise<HeldSettingsPatch> {
  const path = `/api/v1/meetings/${meetingId}`;
  const sent: SentSettingsPatch[] = [];
  const answered: boolean[] = [];
  let released = false;
  let open!: () => void;
  const gate = new Promise<void>((resolve) => {
    open = resolve;
  });
  page.on("response", (response) => {
    const request = response.request();
    if (request.method() === "PATCH" && new URL(request.url()).pathname === path) {
      answered.push(response.ok());
    }
  });
  await page.route(
    (url) => url.pathname === path,
    async (route) => {
      const request = route.request();
      if (request.method() === "PATCH") {
        sent.push({ body: request.postDataJSON(), afterRelease: released });
        if (sent.length === 1) await gate;
      }
      await route.fallback();
    },
  );
  return {
    release: () => {
      released = true;
      open();
    },
    sent,
    answered,
  };
}

function expectConnectedOnce(lines: string[]): void {
  expect(linesWith(lines, CONNECTION_ESTABLISHED_LOG), "connections established").toBe(1);
  expect(linesWith(lines, RECONNECTING_LOG), "reconnect attempts").toBe(0);
}

function prejoinJoinButton(page: Page): Locator {
  return page
    .locator("#join-meeting-container .settings-card")
    .getByRole("button", { name: "Join Meeting", exact: true });
}

function appConfigValue(page: Page, key: string): Promise<unknown> {
  return page.evaluate(
    (k) => (window as Window & { __APP_CONFIG?: Record<string, unknown> }).__APP_CONFIG?.[k],
    key,
  );
}

async function launch(
  uiURL: string,
  user: User | null,
  initScripts: string[] = [],
  prepare?: (context: BrowserContext) => Promise<void>,
): Promise<Session> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  let context: BrowserContext;
  if (user) {
    context = await createAuthenticatedContext(browser, user.email, user.name, uiURL);
  } else {
    context = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
    await context.addInitScript(CERT_HASH_INIT_SCRIPT);
  }
  await pinWebSocketTransport(context);
  for (const script of initScripts) {
    await context.addInitScript(script);
  }
  await prepare?.(context);
  const page = await context.newPage();
  return { browser, context, page };
}

async function showSelfTransportBadge(context: BrowserContext): Promise<void> {
  await setTransportBadgeFlag(context);
  await enableDiagnosticsTileIndicators(context);
}

function launchOnWebTransport(uiURL: string, user: User): Promise<Session> {
  return launch(uiURL, user, [PIN_WEBTRANSPORT_TRANSPORT_INIT_SCRIPT], showSelfTransportBadge);
}

async function expectElectedWebTransport(page: Page): Promise<void> {
  const badge = page.locator(SELF_WT_BADGE);
  await expect(badge, "the participant did not elect WebTransport").toHaveCount(1, {
    timeout: 30_000,
  });
  await expect(badge).toHaveText("WT");
}

function overridesGroup(scope: Locator): Locator {
  return scope.getByRole("group", { name: GROUP_NAME });
}

function overridesTable(scope: Locator): Locator {
  return overridesGroup(scope).getByTestId(OVERRIDES_TABLE_TESTID);
}

function overrideRow(scope: Locator, field: Field): Locator {
  return overridesGroup(scope).getByTestId(`meeting-option-${field}`);
}

function overrideRadioTestId(field: Field, choice: Choice): string {
  return `meeting-option-${field}-${choice}`;
}

function overrideRadio(scope: Locator, field: Field, choice: Choice): Locator {
  return overridesGroup(scope).getByTestId(overrideRadioTestId(field, choice));
}

async function expectOverride(
  scope: Locator,
  field: Field,
  choice: Choice,
  options?: { timeout?: number },
): Promise<void> {
  await expect(overrideRadio(scope, field, choice)).toBeChecked(options);
  for (const other of CHOICES.filter((c) => c !== choice)) {
    await expect(overrideRadio(scope, field, other)).not.toBeChecked();
  }
}

function prejoinCard(page: Page): Locator {
  return page.locator(".settings-card", {
    has: page.getByRole("button", { name: "Start Meeting" }),
  });
}

function overridesSummary(scope: Locator): Locator {
  return overridesGroup(scope).getByTestId("meeting-options-announcements-summary");
}

async function expandOverrides(scope: Locator): Promise<Locator> {
  const disclosure = overridesGroup(scope).locator("details.meeting-options-details");
  await expect(disclosure).toHaveJSProperty("open", false, { timeout: 20_000 });
  await overridesSummary(scope).click();
  await expect(disclosure).toHaveJSProperty("open", true);
  return disclosure;
}

async function expectPrejoinDevicesSettled(page: Page): Promise<void> {
  await expect(page.locator(PREJOIN_CAMERA_OPTIONS)).not.toHaveCount(0, { timeout: 20_000 });
  await expect(page.locator(PREJOIN_PERMISSION_PROMPT)).toHaveCount(0);
}

async function chooseOverride(
  page: Page,
  scope: Locator,
  meetingId: string,
  field: Field,
  choice: Choice,
): Promise<void> {
  const saved = nextSettingsPatch(page, meetingId);
  await overrideRadio(scope, field, choice).check();
  await expectSavedOverride(saved, field, choice);
  await expectOverride(scope, field, choice);
}

function settingsPatchBodies(page: Page, meetingId: string): unknown[] {
  const bodies: unknown[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "PATCH" &&
      new URL(request.url()).pathname === `/api/v1/meetings/${meetingId}`
    ) {
      bodies.push(request.postDataJSON());
    }
  });
  return bodies;
}

async function recordOverrideChanges(page: Page): Promise<void> {
  await page.evaluate((tableTestId) => {
    const w = window as TimingWindow;
    if (w.__vcOverrideChanges) return;
    const seen: OverrideChange[] = [];
    w.__vcOverrideChanges = seen;
    document.addEventListener(
      "change",
      (event) => {
        const input = event.target;
        if (
          input instanceof HTMLInputElement &&
          input.type === "radio" &&
          input.closest(`[data-testid="${tableTestId}"]`)
        ) {
          seen.push({ testId: input.getAttribute("data-testid") ?? "", at: performance.now() });
        }
      },
      true,
    );
  }, OVERRIDES_TABLE_TESTID);
}

function overrideChanges(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const seen = (window as TimingWindow).__vcOverrideChanges;
    if (!seen) throw new Error("the override change recorder is not installed on this page");
    return seen.map((change) => change.testId);
  });
}

function savesAfterLastPickMs(page: Page, meetingId: string): Promise<number[]> {
  return page.evaluate((path) => {
    const w = window as TimingWindow;
    const picks = w.__vcOverrideChanges;
    const starts = w.__vcPatchStarts;
    if (!picks?.length) throw new Error("no override pick was recorded on this page");
    if (!starts) throw new Error("the PATCH start recorder is not installed on this page");
    const lastPickAt = picks[picks.length - 1].at;
    return starts.filter((start) => start.path === path).map((start) => start.at - lastPickAt);
  }, `/api/v1/meetings/${meetingId}`);
}

async function clickInOneTask(page: Page, targets: Locator[]): Promise<void> {
  const handles = await Promise.all(targets.map((target) => target.elementHandle()));
  await page.evaluate((elements) => {
    for (const element of elements) (element as HTMLElement).click();
  }, handles);
  await Promise.all(handles.map((handle) => handle?.dispose()));
}

async function pressInsideOneSettle(page: Page, keys: string[]): Promise<void> {
  const start = Date.now();
  for (const key of keys) {
    await page.keyboard.press(key);
  }
  expect(Date.now() - start, `${keys.join(", ")} spanned a whole settle window`).toBeLessThan(
    OVERRIDE_PICK_SETTLE_MS,
  );
}

function settingsOptionsCard(page: Page): Locator {
  return page.locator(".settings-card", { has: page.getByRole("group", { name: GROUP_NAME }) });
}

async function recordSettingsPageExit(page: Page): Promise<void> {
  await page.evaluate(
    ({ tableTestId, marker }) => {
      const w = window as Window & { __vcLeftAfterPickMs?: number | null };
      w.__vcLeftAfterPickMs = null;
      let pickedAt: number | null = null;
      document.addEventListener(
        "change",
        (event) => {
          const input = event.target;
          if (
            pickedAt === null &&
            input instanceof HTMLInputElement &&
            input.closest(`[data-testid="${tableTestId}"]`)
          ) {
            pickedAt = performance.now();
          }
        },
        true,
      );
      new MutationObserver((_, observer) => {
        if (pickedAt !== null && !document.querySelector(marker)) {
          w.__vcLeftAfterPickMs = performance.now() - pickedAt;
          observer.disconnect();
        }
      }).observe(document.body, { childList: true, subtree: true });
    },
    { tableTestId: OVERRIDES_TABLE_TESTID, marker: SETTINGS_BACK_BUTTON },
  );
}

function settingsLeftAfterPickMs(page: Page): Promise<number | null> {
  return page.evaluate(
    () => (window as Window & { __vcLeftAfterPickMs?: number | null }).__vcLeftAfterPickMs ?? null,
  );
}

function nextSettingsPatch(page: Page, meetingId: string): Promise<Response> {
  return page.waitForResponse(
    (r) =>
      r.request().method() === "PATCH" &&
      new URL(r.url()).pathname === `/api/v1/meetings/${meetingId}`,
  );
}

async function expectSavedOverride(
  saved: Promise<Response>,
  field: Field,
  choice: Choice,
): Promise<void> {
  const response = await saved;
  expect(response.ok(), `PATCH ${field}=${choice}`).toBe(true);
  expect(response.request().postDataJSON()).toEqual({ [WIRE_FIELD[field]]: choice });
}

async function recordPeerToasts(page: Page): Promise<void> {
  await page.evaluate(() => {
    const w = window as Window & { __vcPeerToasts?: Set<Element> };
    if (w.__vcPeerToasts) return;
    const selector = ".peer-toast.toast-joined, .peer-toast.toast-left";
    const seen = new Set<Element>();
    w.__vcPeerToasts = seen;
    const collect = (node: Node) => {
      if (!(node instanceof Element)) return;
      if (node.matches(selector)) seen.add(node);
      node.querySelectorAll(selector).forEach((el) => seen.add(el));
    };
    collect(document.body);
    new MutationObserver((records) => {
      for (const record of records) record.addedNodes.forEach(collect);
    }).observe(document.body, { childList: true, subtree: true });
  });
}

function peerToastsFor(page: Page, kind: ToastKind, name: string): Promise<number> {
  return page.evaluate(
    ({ cls, who }) => {
      const seen = (window as Window & { __vcPeerToasts?: Set<Element> }).__vcPeerToasts;
      if (!seen) throw new Error("the peer-toast recorder is not installed on this page");
      return [...seen].filter(
        (el) => el.classList.contains(cls) && (el.textContent ?? "").includes(who),
      ).length;
    },
    { cls: `toast-${kind}`, who: name },
  );
}

function joinToastsFor(page: Page, name: string): Promise<number> {
  return peerToastsFor(page, "joined", name);
}

function leaveToastsFor(page: Page, name: string): Promise<number> {
  return peerToastsFor(page, "left", name);
}

function peerName(page: Page, who: { name: string }): Locator {
  return page.locator("#grid-container .floating-name", { hasText: who.name }).first();
}

async function enterAsGuest(page: Page, meetingId: string, name: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}/guest`);
  const nameField = page.locator("#guest-name");
  await expect(nameField).toBeVisible({ timeout: 20_000 });
  await nameField.fill(name);
  await nameField.press("Enter");
}

async function enterOnceAdmitted(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const grid = page.locator("#grid-container");
  await expect(joinButton.or(grid).first()).toBeVisible({ timeout: 30_000 });
  await page.waitForTimeout(1000);
  if (!(await grid.isVisible())) {
    await joinButton.click();
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

function storedPref(page: Page, key: string): Promise<string | null> {
  return page.evaluate((k) => localStorage.getItem(k), key);
}

async function openPreferences(page: Page): Promise<void> {
  await wakeActionBar(page);
  await page.locator('[data-testid="open-settings"]').click({ timeout: 10_000 });
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator(".settings-nav-button").filter({ hasText: "Preferences" }).click();
  await expect(page.locator("#settings-panel-preferences")).toBeVisible({ timeout: 5_000 });
}

async function expectHostLocked(page: Page, id: string, forced: boolean): Promise<void> {
  const input = page.locator(id);
  await expect(input).toHaveAttribute("aria-disabled", "true", { timeout: 20_000 });
  await expect(input).toHaveAttribute("aria-describedby", "announce-host-lock");
  await expect(page.locator(`label.glow-switch:has(${id})`)).toHaveClass(/\bglow-switch--locked\b/);
  if (forced) {
    await expect(input).toBeChecked();
  } else {
    await expect(input).not.toBeChecked();
  }
  await expect(page.getByTestId("announce-host-lock")).toBeVisible();
  await expect(page.locator("#announce-host-lock")).toHaveText(
    "Set by the host for this meeting. Your own choice returns when the host changes it back. While you record, join and leave messages still show.",
  );
}

async function expectOwnChoice(page: Page, id: string, own: boolean): Promise<void> {
  const input = page.locator(id);
  await expect(input).not.toHaveAttribute("aria-disabled", { timeout: 20_000 });
  await expect(input).not.toHaveAttribute("aria-describedby");
  await expect(page.locator(`label.glow-switch:has(${id})`)).not.toHaveClass(
    /\bglow-switch--locked\b/,
  );
  if (own) {
    await expect(input).toBeChecked();
  } else {
    await expect(input).not.toBeChecked();
  }
}

function lockStatus(page: Page): Locator {
  return page.getByTestId("announce-host-lock-status");
}

test.describe("Meeting-wide join/leave announcement overrides", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("issue 3168: turning join message and sound off for everyone live locks a participant's switches and silences a new join, and Their choice restores them", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_live_${Date.now()}`;
    const HOST = { email: "ovr3168-live-host@videocall.rs", name: "Ovr3168Host" };
    const FIRST = { email: "ovr3168-live-first@videocall.rs", name: "Ovr3168First" };
    const SECOND = { email: "ovr3168-live-second@videocall.rs", name: "Ovr3168Second" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST, [JOIN_TONE_SPY]);
    const first = await launch(uiURL, FIRST);
    const second = await launch(uiURL, SECOND);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      await recordPeerToasts(host.page);

      await guestJoinsMeeting(host.page, first.page, meetingId, FIRST.name);
      await expect(peerName(host.page, FIRST)).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(() => joinToastsFor(host.page, FIRST.name), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      await expect
        .poll(() => joinChimeCount(host.page), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);

      await openPreferences(first.page);
      await expectOwnChoice(first.page, JOIN_MESSAGE_SWITCH, true);
      await expectOwnChoice(first.page, JOIN_SOUND_SWITCH, true);
      await expect(first.page.locator("#announce-host-lock")).toHaveCount(0);

      const panel = await openMeetingOptions(host.page);
      await expect(overridesTable(panel)).toBeVisible();
      await expect(overrideRow(panel, "join-message").getByRole("radio")).toHaveCount(3);
      await expect(overrideRadio(panel, "join-message", "participant")).toHaveAccessibleName(
        "Join message: Their choice",
      );
      await expect(overrideRadio(panel, "join-message", "on")).toHaveAccessibleName(
        "Join message: On for everyone",
      );
      await expect(overrideRadio(panel, "join-message", "off")).toHaveAccessibleName(
        "Join message: Off for everyone",
      );
      await expectOverride(panel, "join-message", "participant");
      await expectOverride(panel, "join-sound", "participant");

      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await expectHostLocked(first.page, JOIN_MESSAGE_SWITCH, false);
      await expect(lockStatus(first.page)).toHaveText(
        "The host turned join messages off for everyone.",
      );
      await expectOwnChoice(first.page, JOIN_SOUND_SWITCH, true);

      await chooseOverride(host.page, panel, meetingId, "join-sound", "off");
      await expectHostLocked(first.page, JOIN_SOUND_SWITCH, false);
      await expect(lockStatus(first.page)).toHaveText(
        "The host turned join sounds off for everyone.",
      );
      await expectOwnChoice(first.page, LEAVE_MESSAGE_SWITCH, true);

      await recordPeerToasts(first.page);
      const chimesBefore = await joinChimeCount(host.page);
      await guestJoinsMeeting(host.page, second.page, meetingId, SECOND.name);
      await expect(peerName(host.page, SECOND)).toBeVisible({ timeout: 30_000 });
      await expect(peerName(first.page, SECOND)).toBeVisible({ timeout: 30_000 });
      await host.page.waitForTimeout(ANNOUNCE_SETTLE_MS);
      expect(await joinToastsFor(host.page, SECOND.name)).toBe(0);
      expect(await joinToastsFor(first.page, SECOND.name)).toBe(0);
      expect(await joinChimeCount(host.page)).toBe(chimesBefore);

      await chooseOverride(host.page, panel, meetingId, "join-message", "participant");
      await expectOwnChoice(first.page, JOIN_MESSAGE_SWITCH, true);
      await expect(first.page.getByTestId("announce-host-lock")).toBeVisible();

      await chooseOverride(host.page, panel, meetingId, "join-sound", "participant");
      await expectOwnChoice(first.page, JOIN_SOUND_SWITCH, true);
      await expect(first.page.locator("#announce-host-lock")).toHaveCount(0);
      await expect(lockStatus(first.page)).toHaveText("Join sounds are your choice again.");
    } finally {
      await Promise.all([host, first, second].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: in the in-call meeting options, a quick arrow-key sweep across a row saves only its last choice, once, sweeping back to the saved value saves nothing, and the other rows are left alone", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_keys_${Date.now()}`;
    const HOST = { email: "ovr3168-keys-host@videocall.rs", name: "Ovr3168KeysHost" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST, [PATCH_START_RECORDER]);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "leave-sound", "participant");
      await expectOverride(panel, "leave-message", "participant");
      await recordOverrideChanges(host.page);
      const patches = settingsPatchBodies(host.page, meetingId);
      const savedOff = { [WIRE_FIELD["leave-sound"]]: "off" };
      const swept = (choices: Choice[]) =>
        choices.map((choice) => overrideRadioTestId("leave-sound", choice));

      const start = overrideRadio(panel, "leave-sound", "participant");
      await start.focus();
      await expect(start).toBeFocused();
      const saved = nextSettingsPatch(host.page, meetingId);
      await pressInsideOneSettle(host.page, ["ArrowRight", "ArrowRight"]);
      await expectSavedOverride(saved, "leave-sound", "off");
      expect(patches, "only the sweep's last choice is sent").toEqual([savedOff]);
      expect(await overrideChanges(host.page)).toEqual(swept(["on", "off"]));
      const savedAfterMs = await savesAfterLastPickMs(host.page, meetingId);
      expect(savedAfterMs, "PATCH starts seen in the page").toHaveLength(1);
      expect(savedAfterMs[0], "the save did not wait out the settle window").toBeGreaterThanOrEqual(
        OVERRIDE_PICK_SETTLE_MS - SETTLE_TIMER_SLACK_MS,
      );
      expect(savedAfterMs[0], "the settled save was late").toBeLessThan(SETTLED_SAVE_DEADLINE_MS);
      await expectOverride(panel, "leave-sound", "off");
      await expect(overrideRadio(panel, "leave-sound", "off")).toBeFocused();

      await pressInsideOneSettle(host.page, ["ArrowRight", "ArrowLeft"]);
      await host.page.waitForTimeout(NO_SAVE_WINDOW_MS);
      expect(await overrideChanges(host.page)).toEqual(swept(["on", "off", "participant", "off"]));
      expect(patches, "sweeping back to the saved value sends nothing").toEqual([savedOff]);
      await expectOverride(panel, "leave-sound", "off");
      await expect(overrideRadio(panel, "leave-sound", "off")).toBeFocused();
      await expectOverride(panel, "leave-message", "participant");
      await expectOverride(panel, "join-sound", "participant");
    } finally {
      await host.browser.close();
    }
  });

  test("issue 3168: a join-message override picked on the meeting settings page right before Start Meeting leaves it, inside the settle window, is still saved, once", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_leave_${Date.now()}`;
    const HOST = { email: "ovr3168-leave-host@videocall.rs", name: "Ovr3168LeaveHost" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST);
    try {
      await host.page.goto(`/meeting/${meetingId}/settings`);
      const optionsCard = settingsOptionsCard(host.page);
      await expectOverride(optionsCard, "join-message", "participant", { timeout: 20_000 });
      const startMeeting = host.page.getByRole("button", { name: "Start Meeting", exact: true });
      const pickOff = overrideRadio(optionsCard, "join-message", "off");
      await expect(startMeeting).toBeVisible();
      await expect(startMeeting).toBeEnabled();
      await expect(pickOff).toBeEnabled();
      await expect(host.page.locator(SETTINGS_BACK_BUTTON)).toHaveCount(1);
      await recordSettingsPageExit(host.page);
      const patches = settingsPatchBodies(host.page, meetingId);
      const saved = nextSettingsPatch(host.page, meetingId);

      await clickInOneTask(host.page, [pickOff, startMeeting]);
      await expect(host.page.locator(SETTINGS_BACK_BUTTON)).toHaveCount(0);
      const leftAfterMs = await settingsLeftAfterPickMs(host.page);
      expect(leftAfterMs, "the settings page did not unmount after the pick").not.toBeNull();
      expect(
        Number(leftAfterMs),
        "the settings page was left after its settle window",
      ).toBeLessThan(OVERRIDE_PICK_SETTLE_MS);
      await expect(host.page).toHaveURL(new RegExp(`/meeting/${meetingId}$`), { timeout: 10_000 });

      await expectSavedOverride(saved, "join-message", "off");
      await host.page.waitForTimeout(NO_SAVE_WINDOW_MS);
      expect(patches, "the pick is sent once").toEqual([{ [WIRE_FIELD["join-message"]]: "off" }]);

      await host.page.goto(`/meeting/${meetingId}/settings`);
      const reopened = settingsOptionsCard(host.page);
      await expectOverride(reopened, "join-message", "off", { timeout: 20_000 });
      await expectOverride(reopened, "join-sound", "participant");
    } finally {
      await host.browser.close();
    }
  });

  test("issue 3168: on the meeting settings page, a join-sound pick queued behind a held join-message save is still sent after it, once, when Start Meeting leaves the page before the held save lands, and the pre-join card Start Meeting opened shows both saved values without a reload", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_queued_${Date.now()}`;
    const HOST = { email: "ovr3168-queued-host@videocall.rs", name: "Ovr3168QueuedHost" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST, [storedDisplayName(HOST.name)]);
    const held = await holdFirstSettingsPatch(host.page, meetingId);
    try {
      await host.page.goto(`/meeting/${meetingId}/settings`);
      const optionsCard = settingsOptionsCard(host.page);
      await expectOverride(optionsCard, "join-message", "participant", { timeout: 20_000 });
      await expectOverride(optionsCard, "join-sound", "participant");
      const startMeeting = host.page.getByRole("button", { name: "Start Meeting", exact: true });
      await expect(startMeeting).toBeEnabled();

      await overrideRadio(optionsCard, "join-message", "on").check();
      await expect
        .poll(() => held.sent.length, {
          timeout: SETTLED_SAVE_DEADLINE_MS,
          message: "the join-message pick's PATCH never started",
        })
        .toBe(1);
      await overrideRadio(optionsCard, "join-sound", "off").check();
      await expectOverride(optionsCard, "join-sound", "off");
      await expectOverride(optionsCard, "join-message", "on");
      await host.page.waitForTimeout(NO_SAVE_WINDOW_MS);
      expect(held.sent.length, "the join-sound pick was sent while the first save was held").toBe(
        1,
      );

      await startMeeting.click();
      await expect(host.page.locator(SETTINGS_BACK_BUTTON)).toHaveCount(0);
      await expect(host.page).toHaveURL(new RegExp(`/meeting/${meetingId}$`), { timeout: 10_000 });
      expect(held.sent.length, "a PATCH started before the held one was released").toBe(1);
      expect(held.answered, "the held PATCH was answered before its release").toEqual([]);

      const prejoin = prejoinCard(host.page);
      await expectPrejoinDevicesSettled(host.page);
      const disclosure = await expandOverrides(prejoin);
      await expect(overridesTable(prejoin)).toBeVisible();
      await expectOverride(prejoin, "join-message", "participant");
      await expectOverride(prejoin, "join-sound", "participant");
      await disclosure.evaluate((el, mark) => el.setAttribute(mark, ""), SAME_CARD_MARK);

      held.release();
      await expect
        .poll(() => held.answered.length, {
          timeout: 10_000,
          message: "the queued join-sound pick was not sent after the held save landed",
        })
        .toBe(2);
      await host.page.waitForTimeout(NO_SAVE_WINDOW_MS);
      expect(held.sent).toEqual([
        { body: { [WIRE_FIELD["join-message"]]: "on" }, afterRelease: false },
        { body: { [WIRE_FIELD["join-sound"]]: "off" }, afterRelease: true },
      ]);
      expect(held.answered, "both PATCHes succeeded").toEqual([true, true]);
      await expectOverride(prejoin, "join-message", "on", { timeout: 10_000 });
      await expect(disclosure, "the meeting options closed").toHaveJSProperty("open", true);
      await expectOverride(prejoin, "join-sound", "off");
      await expect(disclosure, "the pre-join card was replaced, not updated").toHaveAttribute(
        SAME_CARD_MARK,
        "",
      );

      await host.page.goto(`/meeting/${meetingId}/settings`);
      const reopened = settingsOptionsCard(host.page);
      await expectOverride(reopened, "join-message", "on", { timeout: 20_000 });
      await expectOverride(reopened, "join-sound", "off");
      await expectOverride(reopened, "leave-message", "participant");
    } finally {
      held.release();
      await host.browser.close();
    }
  });

  test("issue 3168: a join-message pick on the pre-join card whose held save lands after browser Back has returned to the meeting settings page shows there without a reload", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_back_${Date.now()}`;
    const HOST = { email: "ovr3168-back-host@videocall.rs", name: "Ovr3168BackHost" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST, [storedDisplayName(HOST.name)]);
    const held = await holdFirstSettingsPatch(host.page, meetingId);
    try {
      await host.page.goto(`/meeting/${meetingId}/settings`);
      const settings = settingsOptionsCard(host.page);
      await expectOverride(settings, "join-message", "participant", { timeout: 20_000 });
      await host.page.getByRole("button", { name: "Start Meeting", exact: true }).click();
      await expect(host.page.locator(SETTINGS_BACK_BUTTON)).toHaveCount(0);
      await expect(host.page).toHaveURL(new RegExp(`/meeting/${meetingId}$`), { timeout: 10_000 });

      const prejoin = prejoinCard(host.page);
      await expectPrejoinDevicesSettled(host.page);
      await expandOverrides(prejoin);
      await expectOverride(prejoin, "join-message", "participant");
      await overrideRadio(prejoin, "join-message", "on").check();
      await expect
        .poll(() => held.sent.length, {
          timeout: SETTLED_SAVE_DEADLINE_MS,
          message: "the pre-join card's join-message PATCH never started",
        })
        .toBe(1);

      await host.page.goBack();
      await expect(host.page).toHaveURL(new RegExp(`/meeting/${meetingId}/settings$`));
      await expect(host.page.locator("#join-meeting-container")).toHaveCount(0, {
        timeout: 10_000,
      });
      await expect(host.page.locator(SETTINGS_BACK_BUTTON)).toHaveCount(1, { timeout: 10_000 });
      await expectOverride(settings, "join-message", "participant", { timeout: 20_000 });
      expect(held.answered, "the held PATCH was answered before its release").toEqual([]);
      await settings.evaluate((el, mark) => el.setAttribute(mark, ""), SAME_CARD_MARK);

      held.release();
      await expect
        .poll(() => held.answered, { timeout: 10_000, message: "the held PATCH never landed" })
        .toEqual([true]);
      await expectOverride(settings, "join-message", "on", { timeout: 10_000 });
      await expect(settings, "the settings page was replaced, not updated").toHaveAttribute(
        SAME_CARD_MARK,
        "",
      );
      expect(held.sent).toEqual([
        { body: { [WIRE_FIELD["join-message"]]: "on" }, afterRelease: false },
      ]);
    } finally {
      held.release();
      await host.browser.close();
    }
  });

  test("issue 3168: join message and sound set on for everyone before the meeting override the host's own off preferences and arrive locked for a late joiner", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_late_${Date.now()}`;
    const HOST = { email: "ovr3168-late-host@videocall.rs", name: "Ovr3168LateHost" };
    const LATE = { email: "ovr3168-late-joiner@videocall.rs", name: "Ovr3168Late" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const ownOff = ownPrefsOff([ENTRY_NOTIFICATIONS_KEY, ENTRY_SOUND_KEY]);
    const host = await launch(uiURL, HOST, [ownOff, JOIN_TONE_SPY]);
    const late = await launch(uiURL, LATE, [ownOff]);
    try {
      await host.page.goto(`/meeting/${meetingId}/settings`);
      const optionsCard = settingsOptionsCard(host.page);
      await expectOverride(optionsCard, "join-message", "participant", { timeout: 20_000 });
      await chooseOverride(host.page, optionsCard, meetingId, "join-message", "on");
      await chooseOverride(host.page, optionsCard, meetingId, "join-sound", "on");

      await fillAndSubmitJoinForm(host.page, meetingId, HOST.name);
      const prejoin = prejoinCard(host.page);
      const disclosure = overridesGroup(prejoin).locator("details.meeting-options-details");
      await expect(disclosure).toHaveJSProperty("open", true, { timeout: 20_000 });
      await expect(overrideRow(prejoin, "join-message")).toBeVisible();
      await expect(overrideRow(prejoin, "join-sound")).toBeVisible();
      await expectOverride(prejoin, "join-message", "on", { timeout: 20_000 });
      await expectOverride(prejoin, "join-sound", "on");
      const summary = overridesSummary(prejoin);
      await expect(summary.getByTestId("meeting-options-announcements-count")).toHaveText("2 set");
      await expect(summary).toHaveAccessibleName(/^Join & leave notifications\s*2 set$/);
      expect(await joinMeetingFromPage(host.page)).toBe("in-meeting");
      expect(await storedPref(host.page, ENTRY_NOTIFICATIONS_KEY)).toBe("false");
      expect(await storedPref(host.page, ENTRY_SOUND_KEY)).toBe("false");
      await recordPeerToasts(host.page);

      await guestJoinsMeeting(host.page, late.page, meetingId, LATE.name);
      await expect(peerName(host.page, LATE)).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(() => joinToastsFor(host.page, LATE.name), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      await expect
        .poll(() => joinChimeCount(host.page), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);

      await openPreferences(late.page);
      await expectHostLocked(late.page, JOIN_MESSAGE_SWITCH, true);
      await expectHostLocked(late.page, JOIN_SOUND_SWITCH, true);
      await expectOwnChoice(late.page, LEAVE_MESSAGE_SWITCH, true);
      await expectOwnChoice(late.page, LEAVE_SOUND_SWITCH, true);
      expect(await storedPref(late.page, ENTRY_NOTIFICATIONS_KEY)).toBe("false");
      expect(await storedPref(late.page, ENTRY_SOUND_KEY)).toBe("false");
    } finally {
      await Promise.all([host, late].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: with join and leave messages off for everyone, a recording host still sees a guest join and leave while a participant who is not recording sees neither, and the guest arrives locked", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_rec_${Date.now()}`;
    const HOST = { email: "ovr3168-rec-host@videocall.rs", name: "Ovr3168RecHost" };
    const FIRST = { email: "ovr3168-rec-first@videocall.rs", name: "Ovr3168RecFirst" };
    const GUEST = { name: "Ovr3168RecGuest" };
    await createMeeting(HOST.email, HOST.name, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: true,
    });

    const host = await launch(uiURL, HOST, [
      ownPrefsOff([ENTRY_NOTIFICATIONS_KEY, EXIT_NOTIFICATIONS_KEY]),
    ]);
    const first = await launch(uiURL, FIRST);
    const guest = await launch(uiURL, null);
    try {
      await fillAndSubmitJoinForm(host.page, meetingId, HOST.name);
      const summary = overridesSummary(prejoinCard(host.page));
      await expect(summary).toBeVisible({ timeout: 20_000 });
      await expect(summary).toHaveAccessibleName(GROUP_NAME);
      await expect(summary.getByTestId("meeting-options-announcements-count")).toHaveCount(0);
      expect(await joinMeetingFromPage(host.page)).toBe("in-meeting");
      expect(await storedPref(host.page, ENTRY_NOTIFICATIONS_KEY)).toBe("false");
      expect(await storedPref(host.page, EXIT_NOTIFICATIONS_KEY)).toBe("false");
      await recordPeerToasts(host.page);

      await guestJoinsMeeting(host.page, first.page, meetingId, FIRST.name);
      await expect(peerName(host.page, FIRST)).toBeVisible({ timeout: 30_000 });
      await recordPeerToasts(first.page);

      await host.page.evaluate("window.showSaveFilePicker = undefined");
      await wakeActionBar(host.page);
      await host.page.getByTestId("record-button").click({ timeout: 10_000 });
      await expectSelfRecording(host.page, "You are recording");

      const panel = await openMeetingOptions(host.page);
      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await chooseOverride(host.page, panel, meetingId, "leave-message", "off");

      await openPreferences(first.page);
      await expectHostLocked(first.page, JOIN_MESSAGE_SWITCH, false);
      await expectHostLocked(first.page, LEAVE_MESSAGE_SWITCH, false);
      await expectOwnChoice(first.page, JOIN_SOUND_SWITCH, true);

      await enterAsGuest(guest.page, meetingId, GUEST.name);
      expect(await joinMeetingFromPage(guest.page)).toBe("in-meeting");
      await expect(peerName(host.page, GUEST)).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(() => joinToastsFor(host.page, GUEST.name), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      await expect(peerName(first.page, GUEST)).toBeVisible({ timeout: 30_000 });
      await first.page.waitForTimeout(ANNOUNCE_SETTLE_MS);
      expect(await joinToastsFor(first.page, GUEST.name)).toBe(0);

      await openPreferences(guest.page);
      await expectHostLocked(guest.page, JOIN_MESSAGE_SWITCH, false);
      await expectHostLocked(guest.page, LEAVE_MESSAGE_SWITCH, false);
      await expectOwnChoice(guest.page, JOIN_SOUND_SWITCH, true);

      await guest.page.close();
      await expect
        .poll(() => leaveToastsFor(host.page, GUEST.name), { timeout: 30_000 })
        .toBeGreaterThanOrEqual(1);
      await expect(peerName(first.page, GUEST)).toHaveCount(0, { timeout: 30_000 });
      await first.page.waitForTimeout(ANNOUNCE_SETTLE_MS);
      expect(await leaveToastsFor(first.page, GUEST.name)).toBe(0);
      await expectSelfRecording(host.page, "You are recording");
    } finally {
      await Promise.all([host, first, guest].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: an attendee and a guest admitted from the waiting room arrive with an override set before they joined already locked in Preferences", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_wr_${Date.now()}`;
    const HOST = { email: "ovr3168-wr-host@videocall.rs", name: "Ovr3168WrHost" };
    const ATTENDEE = { email: "ovr3168-wr-attendee@videocall.rs", name: "Ovr3168WrAttendee" };
    const GUEST = { name: "Ovr3168WrGuest" };
    await createMeeting(HOST.email, HOST.name, {
      meetingId,
      waitingRoomEnabled: true,
      allowGuests: true,
    });
    await patchMeetingSettings(HOST.email, HOST.name, meetingId, { entry_sound_override: "off" });

    const host = await launch(uiURL, HOST);
    const attendee = await launch(uiURL, ATTENDEE);
    const guest = await launch(uiURL, null);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);

      await fillAndSubmitJoinForm(attendee.page, meetingId, ATTENDEE.name);
      expect(await joinMeetingFromPage(attendee.page)).toBe("waiting");
      await enterAsGuest(guest.page, meetingId, GUEST.name);
      await expect(guest.page.getByText("Waiting to be admitted")).toBeVisible({
        timeout: 20_000,
      });

      for (const who of [ATTENDEE, GUEST]) {
        const admit = host.page.getByRole("button", { name: `Admit ${who.name}`, exact: true });
        await expect(admit).toBeVisible({ timeout: 30_000 });
        await admit.dispatchEvent("click");
      }

      for (const admitted of [attendee, guest]) {
        await enterOnceAdmitted(admitted.page);
        await openPreferences(admitted.page);
        await expectHostLocked(admitted.page, JOIN_SOUND_SWITCH, false);
        await expectOwnChoice(admitted.page, JOIN_MESSAGE_SWITCH, true);
      }
    } finally {
      await Promise.all([host, attendee, guest].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: an override the host sets while a participant waits on the pre-join screen, before any connection exists, arrives locked in their Preferences once they join", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_prejoin_${Date.now()}`;
    const HOST = { email: "ovr3168-prejoin-host@videocall.rs", name: "Ovr3168PrejoinHost" };
    const WAITER = { email: "ovr3168-prejoin-waiter@videocall.rs", name: "Ovr3168Waiter" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST);
    const waiter = await launch(uiURL, WAITER, [], (context) =>
      setRuntimeConfig(context, WEBSOCKET_ONLY),
    );
    const hostLog = consoleLines(host.page);
    const waiterLog = consoleLines(waiter.page);
    const waiterReads = statusReads(waiter.page, meetingId);
    const waiterSockets: string[] = [];
    waiter.page.on("websocket", (ws) => {
      if (LOBBY_SOCKET.test(ws.url())) waiterSockets.push(ws.url());
    });
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "join-message", "participant");

      await fillAndSubmitJoinForm(waiter.page, meetingId, WAITER.name);
      const joinButton = prejoinJoinButton(waiter.page);
      await expect(joinButton).toBeVisible({ timeout: 30_000 });
      expect(await appConfigValue(waiter.page, "webTransportEnabled")).toBe("false");

      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await expect
        .poll(() => linesWith(hostLog, SETTINGS_PUSH_LOG), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      await expect(joinButton).toBeVisible();
      await expect(waiter.page.locator("#grid-container")).toHaveCount(0);
      expect(waiterSockets, "the waiter opened a media socket before joining").toEqual([]);
      const readsBeforeJoin = waiterReads.length;

      expect(await joinMeetingFromPage(waiter.page)).toBe("in-meeting");
      expect(waiterSockets.length, "the waiter's media socket was not observed").toBeGreaterThan(0);
      await expect
        .poll(() => linesWith(waiterLog, CONNECTED_LOG), { timeout: 30_000 })
        .toBeGreaterThanOrEqual(1);

      await openPreferences(waiter.page);
      await expectHostLocked(waiter.page, JOIN_MESSAGE_SWITCH, false);
      expectConnectedOnce(waiterLog);
      await expectOwnChoice(waiter.page, JOIN_SOUND_SWITCH, true);
      expect(waiterReads.length, "no settings read after the first connect").toBeGreaterThan(
        readsBeforeJoin,
      );
      expect(linesWith(waiterLog, SETTINGS_PUSH_LOG), "the lock arrived by push").toBe(0);
    } finally {
      await Promise.all([host, waiter].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: when the settings read at a participant's first connect fails, its one retry still brings the override the host set while they waited on the pre-join screen, locked in their Preferences", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_retry_${Date.now()}`;
    const HOST = { email: "ovr3168-retry-host@videocall.rs", name: "Ovr3168RetryHost" };
    const WAITER = { email: "ovr3168-retry-waiter@videocall.rs", name: "Ovr3168RetryWaiter" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST);
    const waiter = await launch(uiURL, WAITER, [], (context) =>
      setRuntimeConfig(context, WEBSOCKET_ONLY),
    );
    const hostLog = consoleLines(host.page);
    const waiterLog = consoleLines(waiter.page);
    const fault = await failFirstArmedStatusRead(waiter.page, meetingId, uiURL);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "join-message", "participant");

      await fillAndSubmitJoinForm(waiter.page, meetingId, WAITER.name);
      const joinButton = prejoinJoinButton(waiter.page);
      await expect(joinButton).toBeVisible({ timeout: 30_000 });
      expect(await appConfigValue(waiter.page, "webTransportEnabled")).toBe("false");

      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await expect
        .poll(() => linesWith(hostLog, SETTINGS_PUSH_LOG), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);

      fault.arm();
      await joinButton.click();
      await expect(waiter.page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
      await expect
        .poll(fault.failed, { timeout: 30_000, message: "no settings read after the Join click" })
        .toBe(1);
      await expect
        .poll(fault.reads, { timeout: 5_000, message: "the failed settings read was not retried" })
        .toBeGreaterThanOrEqual(2);

      await openPreferences(waiter.page);
      await expectHostLocked(waiter.page, JOIN_MESSAGE_SWITCH, false);
      expectConnectedOnce(waiterLog);
      await expectOwnChoice(waiter.page, JOIN_SOUND_SWITCH, true);
      expect(
        linesWith(waiterLog, SETTINGS_READ_FAILED_LOG),
        "the 503 did not fail a settings read",
      ).toBe(1);
      expect(linesWith(waiterLog, SETTINGS_PUSH_LOG), "the lock arrived by push").toBe(0);
    } finally {
      await Promise.all([host, waiter].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: an override the host sets while a participant's connection is down, whose push never reaches them, arrives locked in their Preferences once they reconnect", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_reconnect_${Date.now()}`;
    const HOST = { email: "ovr3168-reconnect-host@videocall.rs", name: "Ovr3168ReconnectHost" };
    const DROPPED = { email: "ovr3168-reconnect-dropped@videocall.rs", name: "Ovr3168Dropped" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    let holding = false;
    let routed = 0;
    let held = 0;
    let capped = false;
    let release!: () => void;
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    const host = await launch(uiURL, HOST);
    const dropped = await launch(uiURL, DROPPED, [], async (context) => {
      await setRuntimeConfig(context, {
        ...WEBSOCKET_ONLY,
        webSocketConnectTimeoutMs: WS_CONNECT_TIMEOUT_MS,
      });
      await installMediaSocketRecorder(context);
      await context.routeWebSocket(LOBBY_SOCKET, async (ws) => {
        routed += 1;
        if (holding) {
          held += 1;
          let cap: ReturnType<typeof setTimeout> | undefined;
          await Promise.race([
            released,
            new Promise<void>((resolve) => {
              cap = setTimeout(() => {
                capped = true;
                resolve();
              }, RECONNECT_HOLD_CAP_MS);
            }),
          ]);
          clearTimeout(cap);
        }
        ws.connectToServer();
      });
    });
    const hostLog = consoleLines(host.page);
    const droppedLog = consoleLines(dropped.page);
    const droppedReads = statusReads(dropped.page, meetingId);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      await guestJoinsMeeting(host.page, dropped.page, meetingId, DROPPED.name);
      expect(await appConfigValue(dropped.page, "webTransportEnabled")).toBe("false");
      expect(await appConfigValue(dropped.page, "webSocketConnectTimeoutMs")).toBe(
        WS_CONNECT_TIMEOUT_MS,
      );
      expect(routed, "the media socket did not go through the route").toBeGreaterThanOrEqual(1);
      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "join-message", "participant");
      await openPreferences(dropped.page);
      await expectOwnChoice(dropped.page, JOIN_MESSAGE_SWITCH, true);
      await expect(dropped.page.locator("#announce-host-lock")).toHaveCount(0);

      const connects = linesWith(droppedLog, CONNECTED_LOG);
      expect(connects).toBeGreaterThanOrEqual(1);
      holding = true;
      const sever = await severMediaWebSocket(dropped.page);
      expect(
        sever.severed,
        `no live media socket closed (the recorder saw ${sever.recorded} socket(s))`,
      ).toBeGreaterThanOrEqual(1);
      await expect
        .poll(() => held, { timeout: 30_000, message: "the client never tried to reconnect" })
        .toBeGreaterThanOrEqual(1);
      expect(linesWith(droppedLog, RECONNECTING_LOG)).toBeGreaterThanOrEqual(1);

      await patchMeetingSettings(HOST.email, HOST.name, meetingId, {
        entry_message_override: "off",
      });
      await expectOverride(panel, "join-message", "off", { timeout: 15_000 });
      await expect
        .poll(() => linesWith(hostLog, SETTINGS_PUSH_LOG), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      expect(linesWith(droppedLog, CONNECTED_LOG), "reconnected before the push").toBe(connects);
      const readsWhileDown = droppedReads.length;
      holding = false;
      release();

      await expect
        .poll(() => linesWith(droppedLog, CONNECTED_LOG), {
          timeout: 45_000,
          message: "the client never reconnected",
        })
        .toBeGreaterThan(connects);
      expect(held, "a held reconnect socket was abandoned and replaced").toBe(1);
      expect(capped, "the hold cap, not release(), let the reconnect through").toBe(false);
      await expect(dropped.page.locator("#settings-panel-preferences")).toBeVisible();
      await expectHostLocked(dropped.page, JOIN_MESSAGE_SWITCH, false);
      await expectOwnChoice(dropped.page, JOIN_SOUND_SWITCH, true);
      expect(droppedReads.length, "no settings read after the reconnect").toBeGreaterThan(
        readsWhileDown,
      );
      expect(linesWith(droppedLog, SETTINGS_PUSH_LOG), "the lock arrived by push").toBe(0);
    } finally {
      release();
      await Promise.all([host, dropped].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: over WebTransport, turning join messages off for everyone live locks a participant's open Preferences by push and silences a new join for them", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_wtlive_${Date.now()}`;
    const HOST = { email: "ovr3168-wtlive-host@videocall.rs", name: "Ovr3168WtHost" };
    const PEER = { email: "ovr3168-wtlive-peer@videocall.rs", name: "Ovr3168WtPeer" };
    const PROBE = { email: "ovr3168-wtlive-probe@videocall.rs", name: "Ovr3168WtProbe" };
    const SECOND = { email: "ovr3168-wtlive-second@videocall.rs", name: "Ovr3168WtSecond" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST);
    const peer = await launchOnWebTransport(uiURL, PEER);
    const probe = await launch(uiURL, PROBE);
    const second = await launch(uiURL, SECOND);
    const peerLog = consoleLines(peer.page);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      await guestJoinsMeeting(host.page, peer.page, meetingId, PEER.name);
      await expectElectedWebTransport(peer.page);
      await recordPeerToasts(peer.page);

      await guestJoinsMeeting(host.page, probe.page, meetingId, PROBE.name);
      await expect(peerName(peer.page, PROBE)).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(() => joinToastsFor(peer.page, PROBE.name), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);

      await openPreferences(peer.page);
      await expectOwnChoice(peer.page, JOIN_MESSAGE_SWITCH, true);
      await expect(peer.page.locator("#announce-host-lock")).toHaveCount(0);
      const pushesBefore = linesWith(peerLog, SETTINGS_PUSH_LOG);

      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "join-message", "participant");
      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await expectHostLocked(peer.page, JOIN_MESSAGE_SWITCH, false);
      await expect(lockStatus(peer.page)).toHaveText(
        "The host turned join messages off for everyone.",
      );
      await expect
        .poll(() => linesWith(peerLog, SETTINGS_PUSH_LOG), {
          timeout: 5_000,
          message: "the push never reached the WebTransport participant",
        })
        .toBeGreaterThan(pushesBefore);
      expectConnectedOnce(peerLog);

      await guestJoinsMeeting(host.page, second.page, meetingId, SECOND.name);
      await expect(peerName(peer.page, SECOND)).toBeVisible({ timeout: 30_000 });
      await peer.page.waitForTimeout(ANNOUNCE_SETTLE_MS);
      expect(await joinToastsFor(peer.page, SECOND.name)).toBe(0);
      await expect(peer.page.locator(SELF_WT_BADGE)).toHaveCount(1);
      expectConnectedOnce(peerLog);
    } finally {
      await Promise.all([host, peer, probe, second].map((s) => s.browser.close()));
    }
  });

  test("issue 3168: over WebTransport, an override the host sets while a participant waits on the pre-join screen, before any connection exists, arrives locked in their Preferences once they join", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_3168_wtprejoin_${Date.now()}`;
    const HOST = { email: "ovr3168-wtprejoin-host@videocall.rs", name: "Ovr3168WtPrejoinHost" };
    const WAITER = { email: "ovr3168-wtprejoin-waiter@videocall.rs", name: "Ovr3168WtWaiter" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const host = await launch(uiURL, HOST);
    const waiter = await launchOnWebTransport(uiURL, WAITER);
    const hostLog = consoleLines(host.page);
    const waiterLog = consoleLines(waiter.page);
    const waiterReads = statusReads(waiter.page, meetingId);
    try {
      await enterMeetingAsHost(host.page, meetingId, HOST.name);
      const panel = await openMeetingOptions(host.page);
      await expectOverride(panel, "join-message", "participant");

      await fillAndSubmitJoinForm(waiter.page, meetingId, WAITER.name);
      const joinButton = prejoinJoinButton(waiter.page);
      await expect(joinButton).toBeVisible({ timeout: 30_000 });

      await chooseOverride(host.page, panel, meetingId, "join-message", "off");
      await expect
        .poll(() => linesWith(hostLog, SETTINGS_PUSH_LOG), { timeout: 15_000 })
        .toBeGreaterThanOrEqual(1);
      await expect(joinButton).toBeVisible();
      await expect(waiter.page.locator("#grid-container")).toHaveCount(0);
      expect(linesWith(waiterLog, CONNECTED_LOG), "the waiter connected before joining").toBe(0);
      const readsBeforeJoin = waiterReads.length;

      expect(await joinMeetingFromPage(waiter.page)).toBe("in-meeting");
      await expect
        .poll(() => linesWith(waiterLog, CONNECTED_LOG), {
          timeout: 30_000,
          message: "the waiter's connection was not observed",
        })
        .toBeGreaterThanOrEqual(1);
      await expectElectedWebTransport(waiter.page);

      await openPreferences(waiter.page);
      await expectHostLocked(waiter.page, JOIN_MESSAGE_SWITCH, false);
      expectConnectedOnce(waiterLog);
      await expectOwnChoice(waiter.page, JOIN_SOUND_SWITCH, true);
      expect(waiterReads.length, "no settings read after the first connect").toBeGreaterThan(
        readsBeforeJoin,
      );
      expect(linesWith(waiterLog, SETTINGS_PUSH_LOG), "the lock arrived by push").toBe(0);
    } finally {
      await Promise.all([host, waiter].map((s) => s.browser.close()));
    }
  });
});
