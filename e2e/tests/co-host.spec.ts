import { test, expect, chromium, Browser, BrowserContext, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { openPeerList, wakeControls } from "../helpers/controls";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { createMeeting } from "../helpers/meeting-api";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * The meeting owner (creator) grants and removes co-hosts. A co-host holds
 * the host role, so it gets host controls and Meeting Options, but not
 * Co-hosts management.
 */

interface User {
  email: string;
  name: string;
}

interface Session {
  browser: Browser;
  context: BrowserContext;
  page: Page;
}

type JoinResult = Awaited<ReturnType<typeof joinMeetingFromPage>>;

const MEETING_OPTIONS_BUTTON = '[data-testid="open-meeting-options"]';
const KICKED_MESSAGE = "You have been removed from the meeting by the host.";

async function launch(uiURL: string, user: User): Promise<Session> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = await createAuthenticatedContext(browser, user.email, user.name, uiURL);
  const page = await context.newPage();
  return { browser, context, page };
}

async function closeAll(sessions: Session[]): Promise<void> {
  await Promise.all(sessions.map((s) => s.browser.close()));
}

async function enterMeeting(page: Page, meetingId: string, user: User): Promise<JoinResult> {
  await fillAndSubmitJoinForm(page, meetingId, user.name);
  return joinMeetingFromPage(page);
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// Mirrors transfer-host.spec.ts's `admitIfNeeded`; the participant side
// reuses `joinMeetingFromPage`, which races the grid against the Join
// button and only clicks Join if the grid hasn't appeared via
// `came_from_waiting_room()` auto-join (dioxus-ui/src/pages/meeting.rs).
async function admitFromWaitingRoom(ownerPage: Page, page: Page, user: User): Promise<void> {
  const admit = ownerPage
    .locator(".waiting-participant", {
      hasText: new RegExp(`${escapeRegExp(user.name)}|${escapeRegExp(user.email)}`),
    })
    .getByTitle("Admit");
  await expect(admit).toBeVisible({ timeout: 20_000 });
  await ownerPage.waitForTimeout(1_000);
  await admit.click();
  await ownerPage.waitForTimeout(3_000);

  expect(await joinMeetingFromPage(page)).toBe("in-meeting");
}

function rosterRow(page: Page, name: string): Locator {
  return page.locator("#peer-list-container .peer_item", { hasText: name }).first();
}

function rosterLabel(page: Page, name: string): Locator {
  return rosterRow(page, name).locator(".peer-indicator");
}

function hostActionsButton(page: Page): Locator {
  return page.locator(
    '#peer-list-container .in-call-header button.menu-button[aria-label="Host actions"]',
  );
}

async function unmute(page: Page): Promise<void> {
  await wakeControls(page);
  const mic = page.getByTestId("mic-toggle-button");
  await expect(mic).toHaveAttribute("aria-label", "Microphone — Unmute", { timeout: 10_000 });
  await mic.click({ timeout: 10_000 });
  await expect(mic).toHaveAttribute("aria-label", "Microphone — Mute", { timeout: 10_000 });
}

// The muted MicIcon draws a (1,1)-(23,23) slash; the unmuted one has none.
async function expectRosterMicOn(page: Page, name: string): Promise<void> {
  const icon = rosterRow(page, name).locator(".peer_item_mic svg");
  await expect(icon).toBeVisible({ timeout: 15_000 });
  await expect(icon.locator('line[x1="1"][y1="1"]')).toHaveCount(0, { timeout: 30_000 });
}

function hostChangeToast(page: Page, text: string): Locator {
  return page.locator(".peer-toast .toast-name", { hasText: text });
}

async function openRowMenu(page: Page, name: string): Promise<Locator> {
  const row = rosterRow(page, name);
  await expect(row).toBeVisible({ timeout: 30_000 });
  const trigger = row.getByTestId("peer-item-menu-button");
  await expect(trigger).toBeVisible({ timeout: 30_000 });
  await trigger.click({ timeout: 10_000 });
  await expect(trigger).toHaveAttribute("aria-expanded", "true", { timeout: 5_000 });
  return row.locator(".peer_item_context_menu .context-menu-item");
}

async function makeCoHostFromRoster(ownerPage: Page, peer: User): Promise<void> {
  const items = await openRowMenu(ownerPage, peer.name);
  await expect(items).toHaveText(["Make co-host", "Transfer host", "Remove from meeting"], {
    timeout: 10_000,
  });
  await rosterRow(ownerPage, peer.name)
    .getByTestId("peer-item-co-host-action")
    .click({ timeout: 10_000 });
}

async function openMeetingOptions(page: Page): Promise<Locator> {
  await wakeControls(page);
  await page.waitForTimeout(300);
  const inline = page.locator(MEETING_OPTIONS_BUTTON);
  await expect(inline).toHaveCount(1, { timeout: 10_000 });
  if (await inline.isVisible()) {
    await inline.click({ timeout: 10_000 });
  } else {
    const trigger = page.locator("#overflow-menu-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click({ timeout: 10_000 });
    const item = page.locator(".action-bar-overflow-popover button.overflow-item", {
      has: page.locator('span:text-is("Meeting options")'),
    });
    await expect(item).toBeVisible({ timeout: 10_000 });
    await item.click({ timeout: 10_000 });
  }
  const panel = page.getByTestId("meeting-options-panel");
  await expect(panel).toBeVisible({ timeout: 10_000 });
  return panel;
}

function coHostRow(section: Locator, user: User): Locator {
  return section.locator(`[data-testid="co-host-row"][data-user-id="${user.email}"]`);
}

async function expectCoHostInCall(
  owner: Session,
  peer: Session,
  ownerUser: User,
  peerUser: User,
): Promise<void> {
  await openPeerList(peer.page);
  await expect(rosterRow(peer.page, peerUser.name)).toBeVisible({ timeout: 15_000 });
  await expect(rosterLabel(peer.page, peerUser.name)).toHaveText("(You/Co-host)", {
    timeout: 45_000,
  });
  await expect(hostActionsButton(peer.page)).toBeVisible({ timeout: 10_000 });
  // A co-host holding the host role also gets Meeting Options.
  await expect(peer.page.locator(MEETING_OPTIONS_BUTTON)).toHaveCount(1, { timeout: 10_000 });

  await openPeerList(owner.page);
  await expect(rosterRow(owner.page, peerUser.name)).toBeVisible({ timeout: 30_000 });
  await expect(rosterLabel(owner.page, peerUser.name)).toHaveText("(Co-host)", {
    timeout: 45_000,
  });
  await expect(rosterLabel(owner.page, ownerUser.name)).toHaveText("(You/Host)", {
    timeout: 10_000,
  });
}

test.describe("Meeting co-hosts", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("owner makes a participant co-host from the roster; they get host controls, the Co-host label, and Meeting Options, but not Co-hosts management @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_grant_${Date.now()}`;
    const OWNER = { email: "cohost-grant-owner@videocall.rs", name: "GrantOwner" };
    const PEER = { email: "cohost-grant-peer@videocall.rs", name: "GrantPeer" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: false });

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("in-meeting");

      await openPeerList(owner.page);
      await openPeerList(peer.page);
      await expect(rosterRow(owner.page, PEER.name)).toBeVisible({ timeout: 30_000 });
      await expect(rosterLabel(owner.page, OWNER.name)).toHaveText("(You/Host)", {
        timeout: 30_000,
      });
      await expect(rosterLabel(owner.page, PEER.name)).toHaveCount(0);
      await expect(rosterRow(peer.page, PEER.name)).toBeVisible({ timeout: 15_000 });
      await expect(rosterLabel(peer.page, PEER.name)).toHaveText("(You)", { timeout: 15_000 });
      await expect(hostActionsButton(peer.page)).toHaveCount(0);
      // A plain participant (not yet host) gets no Meeting Options.
      await expect(peer.page.locator(MEETING_OPTIONS_BUTTON)).toHaveCount(0);

      await makeCoHostFromRoster(owner.page, PEER);

      // Both are 6 s toasts, so poll them together.
      await Promise.all([
        expect(hostChangeToast(peer.page, "You now have host controls")).toBeVisible({
          timeout: 20_000,
        }),
        expect(owner.page.getByTestId("co-host-notice")).toHaveText(
          `${PEER.name} is now a co-host and saved for future meetings.`,
          { timeout: 20_000 },
        ),
      ]);
      await expect(rosterLabel(peer.page, PEER.name)).toHaveText("(You/Co-host)", {
        timeout: 30_000,
      });
      await expect(hostActionsButton(peer.page)).toBeVisible({ timeout: 10_000 });
      await expect(rosterLabel(peer.page, OWNER.name)).toHaveText("(Host)", { timeout: 10_000 });

      await expect(rosterLabel(owner.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });
      await expect(rosterLabel(owner.page, OWNER.name)).toHaveText("(You/Host)");

      await expect(owner.page.locator(MEETING_OPTIONS_BUTTON)).toHaveCount(1, { timeout: 10_000 });

      // A co-host holding the host role gets Meeting Options too (the
      // action-bar slot and the option toggles), but not Co-hosts management.
      await expect(peer.page.locator(MEETING_OPTIONS_BUTTON)).toHaveCount(1, { timeout: 10_000 });
      const peerPanel = await openMeetingOptions(peer.page);
      await expect(
        peerPanel
          .locator(".settings-option-row", {
            has: peer.page.locator(".settings-option-label", { hasText: /^Waiting Room$/ }),
          })
          .getByRole("switch"),
      ).toBeVisible({ timeout: 10_000 });
      await expect(peerPanel.getByTestId("co-hosts-section")).toHaveCount(0);
      await peerPanel
        .getByRole("button", { name: "Close meeting options" })
        .click({ timeout: 10_000 });
      await expect(peerPanel).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await closeAll([owner, peer]);
    }
  });

  test("a co-host gets host actions on a participant but none on the owner, and the owner can remove the co-host", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_powers_${Date.now()}`;
    const OWNER = { email: "cohost-powers-owner@videocall.rs", name: "PowersOwner" };
    const PEER = { email: "cohost-powers-peer@videocall.rs", name: "PowersPeer" };
    const THIRD = { email: "cohost-powers-third@videocall.rs", name: "PowersThird" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: false });

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    const third = await launch(uiURL, THIRD);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      await unmute(owner.page);
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("in-meeting");
      expect(await enterMeeting(third.page, meetingId, THIRD)).toBe("in-meeting");
      await unmute(third.page);

      await openPeerList(owner.page);
      await openPeerList(peer.page);
      await openPeerList(third.page);
      await expect(rosterRow(peer.page, THIRD.name)).toBeVisible({ timeout: 30_000 });
      await expectRosterMicOn(peer.page, THIRD.name);
      await expect(
        rosterRow(peer.page, THIRD.name).getByTestId("peer-item-menu-button"),
      ).toHaveCount(0);

      await makeCoHostFromRoster(owner.page, PEER);
      await expect(rosterLabel(peer.page, PEER.name)).toHaveText("(You/Co-host)", {
        timeout: 30_000,
      });

      await expect(rosterRow(third.page, PEER.name)).toBeVisible({ timeout: 30_000 });
      await expect(rosterLabel(third.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });
      await expect(rosterLabel(third.page, OWNER.name)).toHaveText("(Host)", { timeout: 10_000 });

      await expect(rosterLabel(peer.page, OWNER.name)).toHaveText("(Host)", { timeout: 10_000 });
      await expectRosterMicOn(peer.page, OWNER.name);
      await expect(
        rosterRow(peer.page, OWNER.name).getByTestId("peer-item-menu-button"),
      ).toHaveCount(0);

      const thirdItems = await openRowMenu(peer.page, THIRD.name);
      await expect(thirdItems).toHaveText(["Mute", "Transfer host", "Remove from meeting"], {
        timeout: 10_000,
      });
      await thirdItems.filter({ hasText: "Remove from meeting" }).click({ timeout: 10_000 });
      await expect(
        third.page.locator(".meeting-ended-message", { hasText: KICKED_MESSAGE }),
      ).toBeVisible({ timeout: 20_000 });

      await unmute(peer.page);
      await expectRosterMicOn(owner.page, PEER.name);
      const coHostItems = await openRowMenu(owner.page, PEER.name);
      await expect(coHostItems).toHaveText(["Remove co-host", "Remove from meeting"], {
        timeout: 10_000,
      });
      await coHostItems.filter({ hasText: "Remove from meeting" }).click({ timeout: 10_000 });
      await expect(
        peer.page.locator(".meeting-ended-message", { hasText: KICKED_MESSAGE }),
      ).toBeVisible({ timeout: 20_000 });
    } finally {
      await closeAll([owner, peer, third]);
    }
  });

  test("owner removes a co-host from the roster; they lose host controls", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_revoke_${Date.now()}`;
    const OWNER = { email: "cohost-revoke-owner@videocall.rs", name: "RevokeOwner" };
    const PEER = { email: "cohost-revoke-peer@videocall.rs", name: "RevokePeer" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: false });

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("in-meeting");
      await openPeerList(owner.page);
      await openPeerList(peer.page);

      await makeCoHostFromRoster(owner.page, PEER);
      await expect(rosterLabel(peer.page, PEER.name)).toHaveText("(You/Co-host)", {
        timeout: 30_000,
      });
      await expect(rosterLabel(owner.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });

      const items = await openRowMenu(owner.page, PEER.name);
      await expect(items).toHaveText(["Remove co-host", "Remove from meeting"], {
        timeout: 10_000,
      });
      await rosterRow(owner.page, PEER.name)
        .getByTestId("peer-item-co-host-action")
        .click({ timeout: 10_000 });

      await Promise.all([
        expect(hostChangeToast(peer.page, "You no longer have host controls")).toBeVisible({
          timeout: 20_000,
        }),
        expect(owner.page.getByTestId("co-host-notice")).toHaveText(
          `${PEER.name} is no longer a co-host.`,
          { timeout: 20_000 },
        ),
      ]);
      await expect(rosterLabel(peer.page, PEER.name)).toHaveText("(You)", { timeout: 30_000 });
      await expect(hostActionsButton(peer.page)).toHaveCount(0, { timeout: 10_000 });

      await expect(rosterRow(owner.page, PEER.name)).toBeVisible();
      await expect(rosterLabel(owner.page, PEER.name)).toHaveCount(0, { timeout: 30_000 });
      const after = await openRowMenu(owner.page, PEER.name);
      await expect(after).toHaveText(["Make co-host", "Transfer host", "Remove from meeting"], {
        timeout: 10_000,
      });
    } finally {
      await closeAll([owner, peer]);
    }
  });

  test("a co-host added by id before joining skips the waiting room and joins as a co-host", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_by_id_${Date.now()}`;
    const OWNER = { email: "cohost-byid-owner@videocall.rs", name: "ByIdOwner" };
    const PEER = { email: "cohost-byid-peer@videocall.rs", name: "ByIdPeer" };

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");

      // The owner designates P by id from the meeting settings page (not
      // the in-call dialog), which a co-host can reach before ever joining.
      const settings = await owner.context.newPage();
      await settings.goto(`/meeting/${meetingId}/settings`);
      const waitingRoom = settings
        .locator(".settings-option-row", {
          has: settings.locator(".settings-option-label", { hasText: /^Waiting Room$/ }),
        })
        .getByRole("switch");
      await expect(waitingRoom).toHaveAttribute("aria-checked", "true", { timeout: 10_000 });

      const section = settings.getByTestId("co-hosts-section");
      await expect(section.getByTestId("co-hosts-empty")).toBeVisible({ timeout: 15_000 });
      // Two `.co-hosts-hint` paragraphs coexist here — the section-level one
      // and the add form's own "save for future" note — so each is targeted
      // by its exact text, not the shared class, to avoid a strict-mode
      // violation.
      await expect(
        section.getByText(
          "Co-hosts share your host controls and can change meeting options. Only you can manage co-hosts.",
          { exact: true },
        ),
      ).toBeVisible({ timeout: 15_000 });
      // The add form has no "save for future" checkbox: every grant is
      // saved for future meetings by default.
      await expect(
        section.getByText("New co-hosts are saved for future meetings.", { exact: true }),
      ).toBeVisible({ timeout: 15_000 });
      await section.getByTestId("co-host-input").fill(PEER.email, { timeout: 10_000 });
      await section.getByTestId("co-host-add").click({ timeout: 10_000 });

      const row = coHostRow(section, PEER);
      await expect(row).toBeVisible({ timeout: 15_000 });
      await expect(row.getByRole("switch")).toHaveAttribute("aria-checked", "true");
      await expect(row.getByTestId("co-host-present")).toHaveCount(0);
      await expect(section.getByTestId("co-hosts-status")).toContainText(
        `${PEER.email} was added as a co-host and saved for future meetings.`,
      );

      // P — never admitted — reaches the meeting via the home feed and
      // opens its settings entry before ever joining.
      const peerHome = await peer.context.newPage();
      await peerHome.goto("/");
      const peerRow = peerHome
        .locator(".meetings-list-container .meeting-item")
        .filter({ hasText: meetingId });
      await expect(peerRow).toHaveCount(1, { timeout: 15_000 });
      const peerEditBtn = peerRow.locator(".meeting-edit-btn");
      await expect(peerEditBtn).toHaveCount(1, { timeout: 10_000 });
      await expect(peerEditBtn).toHaveAttribute("title", "Meeting settings (co-host)");
      await peerEditBtn.click({ timeout: 10_000 });
      await expect(peerHome).toHaveURL(new RegExp(`/meeting/${meetingId}/settings`), {
        timeout: 10_000,
      });

      // The creator row is labeled "Owner" (was "Host").
      await expect(
        peerHome
          .locator(".settings-field-compact", {
            has: peerHome.locator(".settings-field-label", { hasText: /^Owner$/ }),
          })
          .locator(".settings-field-value"),
      ).toHaveText(OWNER.name, { timeout: 15_000 });

      // The Co-hosts card is READ-ONLY for P: the list (including their own
      // saved entry) shows, but there is no add form and no Remove button.
      const peerSection = peerHome.getByTestId("co-hosts-section");
      await expect(peerSection).toHaveAttribute("data-read-only", "true", { timeout: 15_000 });
      // read_only hides the add form entirely, so this is the section's only
      // `.co-hosts-hint` today — but target by exact text anyway, matching
      // the owner-view fix above, rather than relying on that staying true.
      await expect(
        peerSection.getByText("Managed by the meeting owner.", { exact: true }),
      ).toBeVisible({ timeout: 15_000 });
      const peerRowInList = coHostRow(peerSection, PEER);
      await expect(peerRowInList).toBeVisible({ timeout: 15_000 });
      await expect(peerRowInList.getByTestId("co-host-saved")).toBeVisible();
      await expect(peerRowInList.getByRole("switch")).toHaveCount(0);
      await expect(peerSection.getByTestId("co-host-input")).toHaveCount(0);
      await expect(peerSection.getByTestId("co-host-add")).toHaveCount(0);
      await expect(peerSection.getByTestId("co-host-remove")).toHaveCount(0);

      // The option toggles stay editable for a co-host — toggle a harmless
      // one (not Waiting Room, which the join below still depends on).
      const allowGuests = peerHome
        .locator(".settings-option-row", {
          has: peerHome.locator(".settings-option-label", { hasText: /^Allow guests$/ }),
        })
        .getByRole("switch");
      await expect(allowGuests).toHaveAttribute("aria-checked", "false", { timeout: 10_000 });
      await allowGuests.click({ timeout: 10_000 });
      await expect(allowGuests).toHaveAttribute("aria-checked", "true", { timeout: 10_000 });

      await peerHome.close();
      await settings.close();

      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("in-meeting");
      await expectCoHostInCall(owner, peer, OWNER, PEER);
    } finally {
      await closeAll([owner, peer]);
    }
  });

  test("a co-host saved for future meetings is a co-host again after the owner ends and restarts the meeting", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_saved_${Date.now()}`;
    const OWNER = { email: "cohost-saved-owner@videocall.rs", name: "SavedOwner" };
    const PEER = { email: "cohost-saved-peer@videocall.rs", name: "SavedPeer" };

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("waiting");
      await admitFromWaitingRoom(owner.page, peer.page, PEER);

      await openPeerList(owner.page);
      await makeCoHostFromRoster(owner.page, PEER);
      await expect(rosterLabel(owner.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });

      const settings = await owner.context.newPage();
      await settings.goto(`/meeting/${meetingId}/settings`);
      const section = settings.getByTestId("co-hosts-section");
      await expect(section).toBeVisible({ timeout: 15_000 });
      // A menu grant with no `persist` is saved for future meetings by
      // default, so the entry starts already saved.
      const save = coHostRow(section, PEER).getByRole("switch");
      await expect(save).toHaveAttribute("aria-checked", "true", { timeout: 15_000 });

      const endButton = settings.getByRole("button", { name: /End Meeting/ });
      await expect(endButton).toBeVisible({ timeout: 10_000 });
      settings.once("dialog", (dialog) => dialog.accept());
      await endButton.click({ timeout: 10_000 });
      await expect(settings.locator(".settings-stat-label", { hasText: /^Ended$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(peer.page.locator(".meeting-ended-message")).toBeVisible({ timeout: 20_000 });

      await settings.close();
      await owner.page.close();
      await peer.page.close();

      const restarted: Session = { ...owner, page: await owner.context.newPage() };
      const rejoined: Session = { ...peer, page: await peer.context.newPage() };
      expect(await enterMeeting(restarted.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(rejoined.page, meetingId, PEER)).toBe("in-meeting");
      await expectCoHostInCall(restarted, rejoined, OWNER, PEER);
    } finally {
      await closeAll([owner, peer]);
    }
  });

  test("a saved co-host restarts the meeting after it ends", async ({ baseURL }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_cohost_restart_${Date.now()}`;
    const OWNER = { email: "cohost-restart-owner@videocall.rs", name: "RestartOwner" };
    const PEER = { email: "cohost-restart-peer@videocall.rs", name: "RestartPeer" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: true });

    const owner = await launch(uiURL, OWNER);
    const peer = await launch(uiURL, PEER);
    try {
      expect(await enterMeeting(owner.page, meetingId, OWNER)).toBe("in-meeting");
      expect(await enterMeeting(peer.page, meetingId, PEER)).toBe("waiting");
      await admitFromWaitingRoom(owner.page, peer.page, PEER);

      await openPeerList(owner.page);
      await makeCoHostFromRoster(owner.page, PEER);
      await expect(rosterLabel(owner.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });

      // A co-host also sees the settings entry point on the home-page
      // meeting list, distinguished from the owner's own title text.
      const peerHome = await peer.context.newPage();
      await peerHome.goto("/");
      const peerRow = peerHome
        .locator(".meetings-list-container .meeting-item")
        .filter({ hasText: meetingId });
      await expect(peerRow).toHaveCount(1, { timeout: 15_000 });
      const peerEditBtn = peerRow.locator(".meeting-edit-btn");
      await expect(peerEditBtn).toHaveCount(1, { timeout: 10_000 });
      await expect(peerEditBtn).toHaveAttribute("title", "Meeting settings (co-host)");
      await peerHome.close();

      // The owner ends the meeting; every page sees MEETING_ENDED.
      const settings = await owner.context.newPage();
      await settings.goto(`/meeting/${meetingId}/settings`);
      const endButton = settings.getByRole("button", { name: /End Meeting/ });
      await expect(endButton).toBeVisible({ timeout: 10_000 });
      settings.once("dialog", (dialog) => dialog.accept());
      await endButton.click({ timeout: 10_000 });
      await expect(settings.locator(".settings-stat-label", { hasText: /^Ended$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(owner.page.locator(".meeting-ended-message")).toBeVisible({ timeout: 20_000 });
      await expect(peer.page.locator(".meeting-ended-message")).toBeVisible({ timeout: 20_000 });

      await settings.close();
      await owner.page.close();
      await peer.page.close();

      // P — not the owner — joins the ended meeting first: a live co-host's
      // own join restarts it, skipping the waiting room and admitting them
      // as host.
      const rejoined: Session = { ...peer, page: await peer.context.newPage() };
      expect(await enterMeeting(rejoined.page, meetingId, PEER)).toBe("in-meeting");

      await openPeerList(rejoined.page);
      await expect(rosterLabel(rejoined.page, PEER.name)).toHaveText("(You/Host)", {
        timeout: 30_000,
      });

      // P, as the restarting host, can open Meeting Options and change one.
      const panel = await openMeetingOptions(rejoined.page);
      const allowGuests = panel
        .locator(".settings-option-row", {
          has: rejoined.page.locator(".settings-option-label", { hasText: /^Allow guests$/ }),
        })
        .getByRole("switch");
      await expect(allowGuests).toHaveAttribute("aria-checked", "false", { timeout: 10_000 });
      await allowGuests.click({ timeout: 10_000 });
      await expect(allowGuests).toHaveAttribute("aria-checked", "true", { timeout: 10_000 });
      await expect(panel.locator(".toggle-error")).toHaveCount(0);
      await panel.getByRole("button", { name: "Close meeting options" }).click({ timeout: 10_000 });
      await expect(panel).toHaveCount(0, { timeout: 10_000 });

      const restarted: Session = { ...owner, page: await owner.context.newPage() };
      expect(await enterMeeting(restarted.page, meetingId, OWNER)).toBe("in-meeting");
      await openPeerList(restarted.page);
      await expect(rosterLabel(restarted.page, OWNER.name)).toHaveText("(You/Host)", {
        timeout: 30_000,
      });
      await expect(rosterLabel(restarted.page, PEER.name)).toHaveText("(Co-host)", {
        timeout: 30_000,
      });
    } finally {
      await closeAll([owner, peer]);
    }
  });
});
