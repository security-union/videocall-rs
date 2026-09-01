import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import {
  MeetingMember,
  MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
  startScreenShare,
} from "../helpers/screen-share-meeting";
import {
  classifyTierChangeKeyframe,
  FORCED_KEYFRAME_LOG,
} from "../helpers/camera-tier-keyframe-log";
import { enableCamera } from "../helpers/controls";

/**
 * A camera AQ tier change must ship a keyframe (issue 2567) — sharer side.
 *
 * Starting a screen share steps the publisher's camera to the "low" tier for
 * cross-stream coordination; without a keyframe the reconfigured stream is
 * undecodable at every receiver until the next periodic one (~5s at kf=100/20fps).
 */

const DEFAULT_UI_URL = "http://localhost:3001";

test.describe("Camera keyframe on AQ tier change — sharer side (issue 2567)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("starting a screen share forces a camera keyframe attributed to the tier change", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_cam_tier_kf_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    const sharerConsole: string[] = [];

    try {
      const profiles = [
        { email: "host-2567@videocall.rs", name: "TierKfHost" },
        { email: "guest-2567@videocall.rs", name: "TierKfGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_TOGGLEABLE_DISPLAY_MEDIA_SCRIPT);
        if (i === 0) {
          ctx.on("page", (p) => {
            p.on("console", (msg) => {
              sharerConsole.push(msg.text());
            });
          });
        }
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page; // sharer
      const guestPage = members[1].page; // viewer

      // The camera is OFF on join, and its encode loop — where the forced keyframe
      // is emitted — only runs while a subscriber is pulling the stream.
      await enableCamera(hostPage);
      await enableCamera(guestPage);
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Precondition, not the assertion: a parked camera encoder would otherwise
      // report the same stage as the regression. This line is `(PLI)`, so it can
      // never satisfy the tier-change assertion below.
      await expect
        .poll(() => sharerConsole.some((l) => l.includes(FORCED_KEYFRAME_LOG)), {
          timeout: 60_000,
          intervals: [500],
          message:
            "the sharer's camera encode loop never emitted a keyframe, so it is not " +
            "encoding — the camera failed to start or no peer subscribed to it",
        })
        .toBe(true);
      await hostPage.waitForTimeout(3000);

      const consoleOffset = sharerConsole.length;

      const shareActivated = await startScreenShare(hostPage, guestPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      await expect
        .poll(() => classifyTierChangeKeyframe(sharerConsole, consoleOffset), {
          timeout: 30_000,
          intervals: [500],
          message:
            "expected the sharer's CameraEncoder to log a forced keyframe attributed to the " +
            "AQ tier change after the screen-share coordination step-down; without it the " +
            "reconfigured camera stream is undecodable at every receiver until the next " +
            "periodic keyframe (~5s at the low tier)",
        })
        .toBe("keyframe-attributed-to-tier-change");
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
