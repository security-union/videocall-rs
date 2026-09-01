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

/**
 * Static-share keyframe FLOOR — sharer side (issue #1903, umbrella of #1832/#1841).
 *
 * ## What this guards
 *
 * When a screen share goes STATIC (a real `getDisplayMedia` track stops emitting
 * frames because the shared content stopped changing) the publisher's encode loop
 * parks on `reader.read()` and its periodic GOP keyframe — which lives in the
 * real-frame branch — never fires. Pre-#1903, the ONLY thing that could re-encode
 * the retained frame while the track was quiet was a pending `KEYFRAME_REQUEST`
 * (PLI) from a receiver (the on-demand #1841 path). So a receiver whose PLI was
 * LOST (WS head-of-line blocking, relay suppression, packet loss) would hold stale
 * content forever — the field's "shared content freezes and never refreshes".
 *
 * The #1903 fix adds an insurance FLOOR: the encode loop's timer branch re-encodes
 * the retained frame as a keyframe on a ~3s wall-clock cadence
 * (`SCREEN_PERIODIC_KEYFRAME_MAX_INTERVAL_MS = 3000ms`), bounded by a post-quiet
 * budget (`SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET = 4`, replenished on every real
 * captured frame) so a truly-idle share backs off after a few cycles. It fires
 * REGARDLESS of any PLI — that is the whole point.
 *
 * ## The scenario: a SHARER + one HAPPY STATIC VIEWER
 *
 * The publisher's screen encoder only actively encodes while there is a subscriber
 * pulling its stream (a lone sharer with zero receivers parks at `target_fps=0`
 * and never produces a retained frame — verified empirically), so the floor needs
 * a receiver present to have anything to re-encode. This spec therefore stands up
 * a real 2-peer meeting: the HOST publishes a mocked screen share and the GUEST
 * views it. During the LIVE phase the guest decodes the keyframe + deltas and its
 * jitter buffer drains to empty (every frame decoded promptly).
 *
 * The sharer emits ONE INFO console line on the first synthetic re-encode
 * (`served_synthetic_once` in `videocall-client/src/encode/screen_encoder.rs`):
 *   "ScreenEncoder: static screen share — re-encoded the retained frame as a
 *    keyframe (on-demand PLI #1841 / wall-clock floor #1903)".
 * We assert that line appears on the SHARER within the floor window after the
 * share goes static.
 *
 * ## Why the line is floor-driven, and fails-on-unfixed
 *
 * That INFO line is shared by the timer branch's on-demand (#1841) and floor
 * (#1903) paths, so it is only a floor witness if NO PLI is pending when it fires.
 * Here it is: once the share is static the guest's jitter buffer is EMPTY (it
 * already decoded a keyframe and every delta), so there is no stale keyframe-less
 * backlog to trip the freshness deadline and no head-age growth to arm the guest's
 * proactive keyframe request (#1025). A happy static viewer sends no PLI. We also
 * wait for the guest to be stably viewing (split layout + settle) BEFORE going
 * static, so any join-time keyframe request is already serviced during the LIVE
 * phase (by the real-frame branch, which does not emit this line).
 *
 * On the pre-#1903 code the quiet timer branch re-encodes ONLY on `pli_pending`;
 * with the viewer sending no PLI it emits nothing while the track is static, the
 * INFO line never appears, and the poll below TIMES OUT and FAILS. With the fix,
 * the floor fires ~3s after the last live keyframe and the line appears.
 * (Reverting the `maybe_floor` / `static_keyframe_floor_due` gate + budget makes
 * this test fail — validated by re-running against the un-fixed build.)
 *
 * ## Behaviour 2 (receiver-side keyframe-less escalation) — already covered
 *
 * The RECEIVER half of the #1903 umbrella — a peer stuck in "deltas flowing, no
 * keyframe" recovering via decoder reset + keyframe re-request within ~6–8s — is
 * ALREADY covered end-to-end by `freshness-skip.spec.ts` ("Test 3", asserting the
 * throttle-bypassed `freshness_skip` with `escalated === true` above the 6000ms
 * `MAX_KEYFRAME_LESS_HOLD_MS` ceiling, "Test 1" the below-ceiling control). This
 * spec deliberately does NOT duplicate it and covers only the new sharer-side floor.
 *
 * The auth + mock + join harness lives in `helpers/screen-share-meeting.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

// The substring that identifies the FLOOR/on-demand synthetic re-encode INFO
// line. With a happy static viewer (no PLI) it is the floor (see doc comment).
const FLOOR_LOG_SUBSTRING = "wall-clock floor #1903";

test.describe("Static screen-share keyframe FLOOR — sharer side (issue #1903)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a paused screen share re-encodes a keyframe on the wall-clock floor (no viewer PLI)", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_floor_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    // Live capture of the SHARER's (host's) browser console. The floor INFO line
    // is emitted by the publisher's ScreenEncoder.
    const sharerConsole: string[] = [];

    try {
      const profiles = [
        { email: "host-1903@videocall.rs", name: "Floor1903Host" },
        { email: "guest-1903@videocall.rs", name: "Floor1903Guest" },
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

      // Host publishes the mocked screen share; the guest must transition to the
      // split layout, proving encoded frames reached it (encoder is live).
      const shareActivated = await startScreenShare(hostPage, guestPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // Phase 1 — LIVE. Let real frames flow (~6s at ~12fps) so the encoder has a
      // retained frame and the floor budget is replenished, and so the guest has
      // fully decoded to an empty jitter buffer (no stale backlog → no PLI later).
      await hostPage.waitForTimeout(6000);

      // Sanity: the encoder actually started (its start log is INFO). Makes a later
      // "no floor line" failure clearly a floor regression, not a share that never
      // began.
      expect(
        sharerConsole.some((l) => l.includes("Start screen share encoder")),
        "expected the screen-share encoder to have started",
      ).toBe(true);

      // Phase 2 — go STATIC on the sharer. Stop requestFrame() ticks: with
      // captureStream(0) the track goes quiet immediately, the encode loop parks on
      // read(), and the timer branch takes over. The guest is a happy static viewer
      // (empty buffer) and sends no PLI, so the only thing that can re-encode the
      // retained frame now is the #1903 floor.
      await hostPage.evaluate(() => {
        (window as unknown as { __e2e1903_emit_frames: boolean }).__e2e1903_emit_frames = false;
      });

      // The floor fires ~3s after the last LIVE keyframe (3s wall-clock cadence) and
      // emits the INFO line on its first synthetic re-encode. Poll for it. 30s is
      // generous headroom over the 3s cadence + the 150ms poll + encoder settle. On
      // the UNFIXED code this line never appears (no PLI ⇒ the quiet timer branch
      // emits nothing) and this poll TIMES OUT.
      await expect
        .poll(() => sharerConsole.filter((l) => l.includes(FLOOR_LOG_SUBSTRING)).length, {
          timeout: 30_000,
          intervals: [500],
          message:
            "expected the ScreenEncoder static-share floor INFO line " +
            `(containing "${FLOOR_LOG_SUBSTRING}") within ~30s of the share going static; ` +
            "its absence means the #1903 wall-clock floor did not re-encode the retained " +
            "frame (a receiver whose PLI was lost would freeze indefinitely).",
        })
        .toBeGreaterThanOrEqual(1);
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
