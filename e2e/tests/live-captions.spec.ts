// Untagged: two browsers per test. Run with `make e2e SPEC=live-captions`.
import { test, expect, chromium, Browser, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { enterTwoUserMeeting, joinMeetingFromPage } from "../helpers/two-user-meeting";
import { openPeerList, wakeActionBar } from "../helpers/controls";
import { CaptionsProducer, KIND } from "../helpers/captions-producer";
import {
  SELF_STATUS_ITEM,
  STATUS_BAR,
  expectSelfItemContained,
  expectSelfRecording,
} from "../helpers/recording-status";

const HOST = "host@videocall.rs";
const GUEST = "guest@videocall.rs";
const NAMES = ["HostUser", "GuestUser"];
const UI = "http://localhost:3001";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";
const id = (testId: string) => `[data-testid="${testId}"]`;

const say = (utteranceId: number, revision: number, isFinal: boolean, text: string) => {
  const lang = { textLanguage: isFinal ? "en" : "it", speakerUserId: Buffer.from(HOST) };
  return { kind: KIND.TEXT, speakerSessionId: 4242, utteranceId, revision, isFinal, text, ...lang };
};

async function captionedRoom(
  flag: boolean,
  body: (host: Page, guest: Page, producer: CaptionsProducer) => Promise<void>,
  storage: Record<string, string> = {},
): Promise<void> {
  const meetingId = `e2e_captions_${Date.now()}`;
  const producer = await CaptionsProducer.start(meetingId);
  const browsers: Browser[] = [];
  try {
    while (browsers.length < 2) browsers.push(await chromium.launch({ args: BROWSER_ARGS }));
    const [host, guest] = await Promise.all(
      [HOST, GUEST].map(async (email, i) => {
        const ctx = await createAuthenticatedContext(browsers[i], email, NAMES[i], UI);
        await setRuntimeConfig(ctx, { captionsEnabled: String(flag) });
        await pinWebSocketTransport(ctx);
        // A saved choice, so the picker's start value does not depend on the
        // machine's locale.
        await ctx.addInitScript((seed) => {
          localStorage.setItem("vc_captions_language", '"en"');
          for (const [k, v] of Object.entries(seed)) localStorage.setItem(k, v);
        }, storage);
        return ctx.newPage();
      }),
    );
    await enterTwoUserMeeting(host, guest, meetingId);
    await body(host, guest, producer);
  } finally {
    await producer.nc.close();
    await Promise.all(browsers.map((b) => b.close()));
  }
}

const cc = (page: Page) => page.locator(id("captions-button"));

async function click(page: Page): Promise<void> {
  await wakeActionBar(page);
  await cc(page).click({ timeout: 10_000 });
}

/** Answers `POST /captions` with `replies` in order, then 204; returns the bodies sent. */
async function stubCaptions(page: Page, replies: Array<[number, string?]> = []) {
  const bodies: unknown[] = [];
  await page.route(/\/api\/v1\/meetings\/[^/]+\/captions$/, (route) => {
    let [status, code]: [number, string?] = [204];
    if (route.request().method() === "POST") {
      bodies.push(route.request().postDataJSON());
      [status, code] = replies.shift() ?? [204];
    }
    return route.fulfill({
      status,
      headers: {
        "access-control-allow-origin": UI,
        "access-control-allow-credentials": "true",
        "access-control-allow-headers": "authorization,content-type",
        "content-type": "application/json",
      },
      body: code ? JSON.stringify({ success: false, result: { code, message: code } }) : "",
    });
  });
  return bodies;
}

/** The inline size the bar's `@container status-bar` queries see. */
const contentWidth = (el: Element) => {
  const cs = getComputedStyle(el);
  const edges = [cs.paddingLeft, cs.paddingRight, cs.borderLeftWidth, cs.borderRightWidth];
  return el.getBoundingClientRect().width - edges.reduce((s, v) => s + parseFloat(v), 0);
};

/** Shown descendants of `bar` that end past its right edge, after ancestor clipping. */
const pastRightEdge = (bar: Element) => {
  const barRight = bar.getBoundingClientRect().right;
  const over: string[] = [];
  const walk = (el: Element, clipRight: number) => {
    for (const child of Array.from(el.children)) {
      const cs = getComputedStyle(child);
      if (cs.display === "none" || cs.clip !== "auto") continue;
      const rect = child.getBoundingClientRect();
      const right = Math.min(rect.right, clipRight);
      if (rect.width > 0 && cs.visibility !== "hidden" && right > barRight + 1) {
        over.push(`${child.tagName}.${child.getAttribute("class")} ends at ${right} > ${barRight}`);
      }
      walk(child, cs.overflowX === "visible" ? clipRight : Math.min(clipRight, rect.right));
    }
  };
  walk(bar, Infinity);
  return over;
};

test.describe("Live captions", () => {
  test.setTimeout(180_000);

  test("flag off: the notice shows but caption text never renders", async () => {
    await captionedRoom(false, async (host, guest, producer) => {
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await producer.send(say(1, 1, true, "Should never render"));
      await producer.status(KIND.STATUS_ACTIVE, GUEST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("GuestUser");
      await expect(guest.locator(id("captions-live-starter"))).toHaveText("you");
      for (const p of [host, guest]) {
        expect(await p.locator(id("caption-line")).count()).toBe(0);
        await expect(cc(p)).toHaveCount(0);
      }
    });
  });

  test("producer packets drive the notice and the caption lane", async () => {
    await captionedRoom(true, async (host, guest, producer) => {
      await stubCaptions(host);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("you");
      await expect(guest.locator(id("captions-live-starter"))).toHaveText("HostUser");
      await producer.send(say(1, 1, false, "Buongiorno a tutti"));
      for (const p of [host, guest]) {
        await expect(p.locator(id("caption-line"))).toHaveAttribute("data-final", "false");
        await expect(p.locator(id("caption-line-lang"))).toHaveText("IT");
      }
      await producer.send(say(1, 2, true, "Good morning everyone"));
      for (const p of [host, guest]) {
        await expect(p.locator(id("caption-line"))).toHaveAttribute("data-final", "true");
        await expect(p.locator(`${id("captions-live-region")} p`)).toHaveText([
          `${HOST}: Good morning everyone`,
        ]);
      }

      // Issue 3126: recording beside the notice keeps ONE recording status.
      await host.evaluate("window.showSaveFilePicker = undefined");
      await wakeActionBar(host);
      await host.getByTestId("record-button").click({ timeout: 10_000 });
      await expectSelfRecording(host, "You are recording");
      const recording = host.locator('[role="status"]').filter({ hasText: /recording/i });
      await expect(recording).toHaveCount(1);
      await expect(recording).toHaveClass(/\bmeeting-status-item--self\b/);
      await expect(host.locator(STATUS_BAR).locator(id("captions-live-indicator"))).toBeVisible();
      await expectSelfItemContained(host);

      // On a phone the file name gives way and the picker becomes a code chip.
      const bar = host.locator(STATUS_BAR);
      const target = host.locator(SELF_STATUS_ITEM).locator(":scope > .rec-target");
      const code = bar.locator(".captions-language__code");
      const picker = bar.locator(id("captions-language-select"));
      await expect(target).toBeVisible();
      await host.setViewportSize({ width: 360, height: 740 });
      await expect(bar).toBeVisible();
      await expect.poll(() => bar.evaluate(contentWidth)).toBeLessThan(379);
      await expectSelfRecording(host, /You are recording/);
      await expectSelfItemContained(host);
      await expect(target).toHaveCSS("display", "none");
      await expect(code).toBeVisible();
      await expect(code).toHaveText("EN");
      await expect(picker).toBeEnabled();
      await expect.poll(() => bar.evaluate(pastRightEdge)).toEqual([]);
      await picker.click({ trial: true });
      await picker.selectOption("it");
      await expect(code).toHaveText("IT");

      await producer.status(KIND.STATUS_STOPPED);
      for (const p of [host, guest]) {
        await expect(p.locator(id("captions-live-indicator"))).toHaveCount(0);
        await expect(p.locator(id("captions-overlay"))).toHaveCount(0);
      }
    });
  });

  test("a viewer picks a caption language and sees that translation", async () => {
    await captionedRoom(true, async (host, guest, producer) => {
      await stubCaptions(host);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      const picker = guest.locator(id("captions-language-select"));
      await expect(picker).toHaveValue("en");
      await producer.send({
        ...say(1, 1, true, "Good morning everyone"),
        translations: [
          { language: "en", text: "Good morning everyone" },
          { language: "it", text: "Buongiorno a tutti" },
          { language: "de", text: "Guten Morgen zusammen" },
        ],
      });
      const text = (p: Page) => p.locator(id("caption-line-text"));
      await expect(text(guest)).toHaveText("Good morning everyone");
      await picker.selectOption("it");
      await expect(text(guest)).toHaveText("Buongiorno a tutti");
      await expect(text(guest)).toHaveAttribute("lang", "it");
      await expect(guest.locator(id("caption-line-lang"))).toHaveCount(0);
      await expect(guest.locator(`${id("captions-live-region")} p`)).toHaveText([
        `${HOST}: Buongiorno a tutti`,
      ]);
      await expect(guest.locator(`${id("captions-live-region")} p`)).toHaveAttribute("lang", "it");
      await expect(text(host)).toHaveText("Good morning everyone");
      await picker.selectOption("fr");
      await expect(text(guest)).toHaveText("Good morning everyone");
      await expect(guest.locator(id("caption-line-lang"))).toHaveText("EN");
    });
  });

  test("host starts, renews and revokes; a viewer hides and shows the lane", async () => {
    test.setTimeout(240_000);
    await captionedRoom(true, async (host, guest, producer) => {
      const posts = await stubCaptions(host, [[204], [204], [204], [404, "NOT_FOUND"]]);
      const guestPosts = await stubCaptions(guest);
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await expect(cc(guest)).toHaveAttribute("data-captions-role", "viewer");
      await expect(cc(guest)).toHaveAttribute("aria-disabled", "true");

      await click(host);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
      await expect.poll(() => posts).toEqual([{ enabled: true, renew: false }]);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect(cc(guest)).toHaveAttribute("aria-pressed", "true");
      await click(guest);
      await expect(cc(guest)).toHaveAttribute("aria-pressed", "false");
      await expect(guest.locator(id("captions-overlay"))).toHaveCount(0);
      await producer.send(say(1, 1, true, "Hidden from the guest"));
      await expect(host.locator(id("caption-line-text"))).toHaveText(["Hidden from the guest"]);
      // The guest has handled the caption once it shows the status sent after it.
      await producer.status(KIND.STATUS_ACTIVE, GUEST);
      await expect(guest.locator(id("captions-live-starter"))).toHaveText("you");
      await click(guest);
      await expect(cc(guest)).toHaveAttribute("aria-pressed", "true");
      expect(await guest.locator(id("caption-line")).count()).toBe(0);
      await producer.send(say(2, 1, true, "Shown again"));
      await expect(guest.locator(id("caption-line-text"))).toHaveText(["Shown again"]);

      await producer.status(KIND.STATUS_ACTIVE, HOST);
      const beat = setInterval(
        () => producer.status(KIND.STATUS_ACTIVE, HOST).catch(() => {}),
        10_000,
      );
      try {
        await expect.poll(() => posts.length, { timeout: 60_000 }).toBe(2);
        expect(posts[1]).toEqual({ enabled: true, renew: true });
      } finally {
        clearInterval(beat);
      }

      await click(host);
      await expect.poll(() => posts[2]).toEqual({ enabled: false, renew: false });
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await host.waitForTimeout(3_000);
      expect(posts).toHaveLength(3);
      await producer.status(KIND.STATUS_STOPPED);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "false");
      await expect(cc(guest)).toHaveAttribute("aria-disabled", "true");
      await click(host);
      await expect.poll(() => posts[3]).toEqual({ enabled: true, renew: false });
      await expect(cc(host)).toHaveCount(0);
      expect(guestPosts).toEqual([]);
    });
  });

  test("a host who rejoins an active lease renews it instead of re-enabling", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      const posts = await stubCaptions(host);
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
      await expect.poll(() => posts).toEqual([{ enabled: true, renew: true }]);
      await producer.status(KIND.STATUS_STOPPED);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "false");
    });
  });

  test("a host who reloads during their lease renews it after rejoining", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      const posts = await stubCaptions(host);
      await click(host);
      await expect.poll(() => posts).toEqual([{ enabled: true, renew: false }]);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("you");

      await host.reload();
      expect(await joinMeetingFromPage(host)).toBe("in-meeting");
      const beat = setInterval(
        () => producer.status(KIND.STATUS_ACTIVE, HOST).catch(() => {}),
        2_000,
      );
      try {
        await expect.poll(() => posts.length, { timeout: 30_000 }).toBe(2);
      } finally {
        clearInterval(beat);
      }
      expect(posts[1]).toEqual({ enabled: true, renew: true });
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
    });
  });

  test("a rejected adoption is not retried on the next heartbeat", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      const posts = await stubCaptions(host, [[409, "CAPTIONS_OFF"]]);
      const renew = { enabled: true, renew: true };
      const rejected = host.waitForEvent("console", (m) =>
        m.text().includes("captions are off or the meeting is not active"),
      );
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect.poll(() => posts).toEqual([renew]);
      await rejected;
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await host.waitForTimeout(3_000);
      expect(posts).toEqual([renew]);
      await producer.status(KIND.STATUS_STOPPED);
      await expect(host.locator(id("captions-live-indicator"))).toHaveCount(0);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect.poll(() => posts).toEqual([renew, renew]);
    });
  });

  test("MEETING_NOT_ACTIVE drops the host's on-state and NOT_HOST demotes only while the room is captioned", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      const notHost: [number, string] = [403, "NOT_HOST"];
      const posts = await stubCaptions(host, [
        [409, "MEETING_NOT_ACTIVE"],
        notHost,
        notHost,
        notHost,
      ]);
      const enable = { enabled: true, renew: false };
      await click(host);
      await expect.poll(() => posts.length).toBe(1);
      await expect(cc(host)).toHaveAttribute("data-captions-pending", "false");
      await expect(cc(host)).toHaveAttribute("aria-pressed", "false");

      await click(host);
      await expect.poll(() => posts[1]).toEqual(enable);
      await expect(cc(host)).toHaveAttribute("data-captions-pending", "false");
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await expect(cc(host)).not.toHaveAttribute("aria-disabled", "true");
      await click(host);
      await expect.poll(() => posts[2]).toEqual(enable);
      await expect(cc(host)).toHaveAttribute("data-captions-pending", "false");
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");

      await producer.status(KIND.STATUS_ACTIVE, GUEST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("GuestUser");
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
      await click(host);
      await expect.poll(() => posts[3]).toEqual({ enabled: false, renew: false });
      await expect(cc(host)).toHaveAttribute("data-captions-role", "viewer");
      await expect(host.locator(id("captions-live-indicator"))).toHaveCount(1);

      await producer.status(KIND.STATUS_STOPPED);
      await expect(host.locator(id("captions-live-indicator"))).toHaveCount(0);
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      expect(posts).toHaveLength(4);
    });
  });

  test("a latched host's start reported active before its 204 keeps the host's control", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      const notHost: [number, string] = [403, "NOT_HOST"];
      const posts = await stubCaptions(host, [notHost, notHost]);
      await click(host);
      await expect.poll(() => posts.length).toBe(1);
      await expect(cc(host)).toHaveAttribute("data-captions-pending", "false");
      await producer.status(KIND.STATUS_ACTIVE, GUEST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("GuestUser");
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await click(host);
      await expect.poll(() => posts[1]).toEqual({ enabled: false, renew: false });
      await expect(cc(host)).toHaveAttribute("data-captions-role", "viewer");
      await expect(host.locator(id("captions-live-indicator"))).toHaveCount(0, {
        timeout: 45_000,
      });
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");

      let release = () => {};
      const held = new Promise<void>((resolve) => (release = resolve));
      let seen = 0;
      await host.route(/\/api\/v1\/meetings\/[^/]+\/captions$/, async (route) => {
        if (route.request().method() === "POST") {
          seen += 1;
          await held;
        }
        await route.fallback();
      });
      await click(host);
      await expect.poll(() => seen).toBe(1);
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await expect(host.locator(id("captions-live-starter"))).toHaveText("you");
      release();
      await expect.poll(() => posts[2]).toEqual({ enabled: true, renew: false });
      await expect(cc(host)).toHaveAttribute("data-captions-role", "host");
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
    });
  });

  test("a rejected start tells the host why captions did not start", async () => {
    await captionedRoom(true, async (host) => {
      const posts = await stubCaptions(host, [[403, "NOT_HOST"], [204]]);
      const notice = host.locator(id("co-host-notice"));
      await click(host);
      await expect(notice).toHaveText("Couldn't start captions. Only hosts can do that.");
      await expect(notice).toHaveAttribute("role", "alert");
      await expect(cc(host)).toHaveAttribute("aria-pressed", "false");
      await expect(notice).toHaveCount(0, { timeout: 10_000 });

      await click(host);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "true");
      expect(posts[1]).toEqual({ enabled: true, renew: false });
      await expect(notice).toHaveCount(0);
    });
  });

  test("a revoke answered 503 is sent again until it is accepted", async () => {
    await captionedRoom(true, async (host) => {
      const posts = await stubCaptions(host, [[204], [503, "CAPTIONS_UNAVAILABLE"], [204]]);
      const enable = { enabled: true, renew: false };
      const revoke = { enabled: false, renew: false };
      await click(host);
      await expect.poll(() => posts).toEqual([enable]);
      await click(host);
      await expect(cc(host)).toHaveAttribute("aria-pressed", "false");
      await expect.poll(() => posts, { timeout: 10_000 }).toEqual([enable, revoke, revoke]);
      // Longer than the next backoff step (CAPTIONS_REVOKE_RETRY_MS[1] = 2 s), so
      // a loop that kept resending after the 204 would show up here.
      await host.waitForTimeout(3_000);
      expect(posts).toEqual([enable, revoke, revoke]);
    });
  });

  test("meeting-api publishes the host's lease and its revoke", async ({ request }) => {
    test.skip(process.env.CAPTIONS_ENABLED === "false", "CAPTIONS_ENABLED is off for this stack");
    const probe = await request.post(`${API_URL}/api/v1/meetings/e2e/captions`, {
      data: { enabled: true },
    });
    const body = await probe.text();
    test.skip(probe.status() === 404 && body === "", "meeting-api serves no captions route");
    expect(probe.status(), `captions route present but disabled: ${body}`).not.toBe(404);
    await captionedRoom(true, async (host, _guest, producer) => {
      const leases: Array<Record<string, unknown>> = [];
      producer.nc.subscribe(`captions.control.${producer.room}`, {
        callback: (_err, msg) => leases.push(msg.json()),
      });
      await producer.nc.flush();
      await click(host);
      await expect.poll(() => leases.length).toBe(1);
      expect(leases[0]).toMatchObject({ enabled: true, requested_by: HOST, ttl_secs: 120 });
      await click(host);
      await expect.poll(() => leases.length).toBe(2);
      const generation = (leases[0].generation as number) + 1;
      expect(leases[1]).toMatchObject({ enabled: false, revoke: true, generation });
    });
  });

  type Box = { x: number; y: number; width: number; height: number };
  const apart = (a: Box | null, b: Box | null) =>
    !!a &&
    !!b &&
    (a.x + a.width <= b.x ||
      b.x + b.width <= a.x ||
      a.y + a.height <= b.y ||
      b.y + b.height <= a.y);
  const laneClears = async (page: Page, selectors: string[]) => {
    await expect
      .poll(async () => {
        const lane = await page.locator(id("captions-overlay")).boundingBox();
        const boxes = await Promise.all(
          selectors.map((s) => page.locator(s).first().boundingBox()),
        );
        return [lane !== null && lane.x + lane.width <= 900, ...boxes.map((b) => apart(lane, b))];
      })
      .toEqual(selectors.map(() => true).concat(true));
  };
  const midWidthRoom = async (
    storage: Record<string, string>,
    body: (guest: Page, caption: (n: number, repeat?: number) => Promise<void>) => Promise<void>,
  ) =>
    captionedRoom(
      true,
      async (host, guest, producer) => {
        await stubCaptions(host);
        await guest.setViewportSize({ width: 900, height: 720 });
        await producer.status(KIND.STATUS_ACTIVE, HOST);
        await expect(guest.locator(id("captions-live-starter"))).toHaveText("HostUser");
        const self = guest.locator("#host-controls-nav");
        await expect(self).toHaveAttribute("data-self-placement", "corner");
        await expect(self).toHaveAttribute("data-self-hidden", "false");
        await body(guest, async (n, repeat = 4) => {
          await producer.send(
            say(n, 1, true, `Caption ${n} long enough to fill the lane `.repeat(repeat)),
          );
          await expect(guest.locator(id("caption-line-text")).last()).toContainText(`Caption ${n}`);
        });
      },
      storage,
    );

  test("on a 1920px screen the lane is 720-1120px wide and shows four 18px rows", async () => {
    await midWidthRoom({}, async (guest, caption) => {
      await guest.setViewportSize({ width: 1920, height: 1080 });
      await caption(1, 20);
      const lane = guest.locator(id("captions-overlay"));
      const self = guest.locator("#host-controls-nav");
      const text = guest.locator(id("caption-line-text")).last();
      await expect.poll(async () => (await lane.boundingBox())?.width ?? 0).toBeGreaterThan(720);
      expect((await lane.boundingBox())!.width).toBeLessThanOrEqual(1120);
      await expect
        .poll(async () => apart(await lane.boundingBox(), await self.boundingBox()))
        .toBe(true);
      await expect(text).toHaveCSS("font-size", "18px");
      expect((await text.boundingBox())!.height / (1.4 * 18)).toBeCloseTo(4, 1);
    });
  });

  test("an open tile host-actions menu is never under the caption lane", async () => {
    await captionedRoom(true, async (host, _guest, producer) => {
      await stubCaptions(host);
      await host.setViewportSize({ width: 1920, height: 1080 });
      await producer.status(KIND.STATUS_ACTIVE, HOST);
      await producer.send(say(1, 1, true, "Over the tile menu ".repeat(40)));
      const lane = host.locator(id("captions-overlay"));
      await expect(host.locator(id("caption-line-text"))).toContainText("Over the tile menu");
      await expect(lane).toBeVisible();

      const tile = host.locator(".grid-item:has(.tile-mute-btn)").first();
      await tile.hover();
      await tile.getByTitle("Host actions").click();
      const kick = tile.locator(".tile-context-menu-item", { hasText: "Remove from meeting" });
      await expect(kick).toBeVisible();
      await expect(lane).toHaveCSS("visibility", "hidden");

      await host.keyboard.press("Escape");
      await expect(kick).toHaveCount(0);
      await expect(lane).toHaveCSS("visibility", "visible");
    });
  });

  test("at a mid width the lane clears an open drawer and the corner self view", async () => {
    await midWidthRoom({}, async (guest, caption) => {
      await openPeerList(guest);
      await caption(1);
      await laneClears(guest, ["#peer-list-container", "#host-controls-nav"]);

      await guest.locator("#peer-list-container button.close-button").click();
      await expect(guest.locator("#peer-list-container")).not.toHaveClass(/\bvisible\b/);
    });
  });

  test("at a mid width the lane clears a left dock beside the open peer list", async () => {
    await midWidthRoom({ vc_dock_position: "left" }, async (guest, caption) => {
      await expect(guest.locator(".video-controls-container")).toHaveClass(/\bdock-left\b/);
      await openPeerList(guest);
      await caption(1);
      await wakeActionBar(guest);
      await laneClears(guest, [
        "#peer-list-container",
        ".video-controls-container",
        "#host-controls-nav",
      ]);
    });
  });

  test("below 568px with a right dock the lane clears the show-self icon", async () => {
    const storage = { vc_dock_position: "right", vc_self_view_visible: "false" };
    await captionedRoom(
      true,
      async (host, guest, producer) => {
        await stubCaptions(host);
        await guest.setViewportSize({ width: 500, height: 800 });
        await producer.status(KIND.STATUS_ACTIVE, HOST);
        await expect(guest.locator(id("captions-live-starter"))).toHaveText("HostUser");
        const icon = guest.locator("button.self-view-hidden-icon--dock-right");
        await expect(icon).toBeVisible();
        await producer.send(say(1, 1, true, "A caption long enough to fill the lane ".repeat(4)));
        await expect(guest.locator(id("caption-line-text")).last()).toContainText("A caption");
        const lane = guest.locator(id("captions-overlay"));
        await expect
          .poll(async () => apart(await lane.boundingBox(), await icon.boundingBox()))
          .toBe(true);
      },
      storage,
    );
  });
});
