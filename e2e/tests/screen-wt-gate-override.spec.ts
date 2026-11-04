import { test, expect, chromium, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebTransportTransport,
} from "../helpers/auth-context";
import { TimedLine, collectTimedConsoleFromContext } from "../helpers/camera-ws-gate";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import {
  distinctChecksumsInWindow,
  sampleScreenShareChecksum,
  sampleScreenShareChecksumSeries,
} from "../helpers/frame-liveness";
import {
  MeetingMember,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
  startScreenShare,
} from "../helpers/screen-share-meeting";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import { waitForServices } from "../helpers/wait-for-services";

const DEFAULT_UI_URL = "http://localhost:3001";

// 1920x1080 at the 10 fps screen tier: baseline 2488 kbps, so on a local RTT the
// WT delta threshold is 311,000 B and the keyframe ceiling 933,000 B.
// captureStream(0) emits only on requestFrame(), so clearing `animate` makes the share static.
const SHARE_MOCK = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true,
      value: async () => {
        const c = document.createElement('canvas');
        c.width = 1920; c.height = 1080;
        const ctx = c.getContext('2d');
        const stream = c.captureStream(0);
        const track = stream.getVideoTracks()[0];
        let n = 0;
        const paint = () => {
          n++;
          ctx.fillStyle = '#1a1a2e';
          ctx.fillRect(0, 0, 1920, 1080);
          ctx.fillStyle = 'hsl(' + ((n * 37) % 360) + ', 70%, 45%)';
          ctx.fillRect(720, 405, 480, 270);
          ctx.fillStyle = '#fff'; ctx.font = '48px sans-serif';
          ctx.fillText('WT screen gate ' + n, 760, 560);
          track.requestFrame();
        };
        const whiteSlide = () => {
          ctx.fillStyle = '#1a1a2e';
          ctx.fillRect(0, 0, 1920, 1080);
          ctx.fillStyle = '#fff';
          ctx.fillRect(720, 405, 480, 270);
          track.requestFrame();
        };
        window.__e2e3077 = { animate: true, showWhiteSlide: whiteSlide };
        paint();
        setInterval(() => { if (window.__e2e3077.animate) paint(); }, 100);
        return stream;
      },
    });
  })();
`;

const DELTA_DROP_OVERRIDE_BYTES = 400_000;
const CEILING_OVERRIDE_BYTES = 2_000_000;

const WT_DELTA_DROP_RE =
  /ScreenEncoder: dropping screen delta\(s\) on the WT screen stream \(issue #3077\) — dropped=\d+ in last window, queued=(\d+), threshold=(\d+), debt=([a-z_]+)/;
const WT_KEYFRAME_DROP_RE =
  /ScreenEncoder: dropping a screen keyframe over the WT queue ceiling \(issue #3077\) — queued=(\d+), ceiling=(\d+); a fresh keyframe is owed/;
const WT_DRAINED_RE =
  /ScreenEncoder: WT screen queue drained \(queued=(\d+), threshold=(\d+)\) — requesting the owed keyframe \(issue #3077\)/;
const WT_OWED_EMIT_RE =
  /ScreenEncoder: emitting the keyframe owed since a WT queue-ceiling drop \(issue #3077\)/;
const WT_STATIC_OWED_RE =
  /ScreenEncoder: static share — re-encoding the retained frame as the keyframe owed since a WT queue drop \(queued=(\d+), threshold=(\d+)\) \(issue #3077\)/;

const SELF_WT_BADGE = '.transport-badge[aria-label="Your connection transport: WebTransport"]';
const RECEIVED_SHARE_CANVAS = '[data-testid="received-share-tile"] canvas[id^="screen-share-"]';

const GATE_LINE_TIMEOUT_MS = 10_000;
const KEYFRAME_REACH_TIMEOUT_MS = 10_000;
const LIVE_REACH_TIMEOUT_MS = 15_000;
const IN_FLIGHT_DRAIN_MS = 1_000;
const BASELINE_QUIET_MS = 2_000;
const FREEZE_WINDOW_MS = 4_000;
const LIVE_WINDOW_MS = 3_000;
const LIVE_MIN_DISTINCT = 8;
const KEYFRAME_ONLY_MAX_DISTINCT = 7;
const WINDOW_MIN_SAMPLES = 10;
const FLOOR_BUDGET = 4;
const FLOOR_SPENT_QUIET_MS = 4_500;
const FLOOR_SPENT_TIMEOUT_MS = 30_000;
// sampleScreenShareChecksum sums 241 bytes of a 32x32 RGBA patch: an all-white patch is 61,455.
const WHITE_SLIDE_MIN_SUM = 58_000;
const TEARDOWN_TIMEOUT_MS = 5_000;

type WtScreenHook = { setWtScreenQueueOverride?: (bytes: number | null) => unknown };
type ShareMock = { __e2e3077?: { animate: boolean; showWhiteSlide: () => void } };

interface Hit {
  index: number;
  m: RegExpExecArray;
}

function hits(lines: TimedLine[], re: RegExp, from = 0): Hit[] {
  const out: Hit[] = [];
  for (let i = from; i < lines.length; i++) {
    const m = re.exec(lines[i].text);
    if (m) out.push({ index: i, m });
  }
  return out;
}

async function waitForHit(
  lines: TimedLine[],
  re: RegExp,
  from: number,
  message: string,
  accept: (m: RegExpExecArray) => boolean = () => true,
): Promise<Hit> {
  await expect
    .poll(() => hits(lines, re, from).some((h) => accept(h.m)), {
      timeout: GATE_LINE_TIMEOUT_MS,
      intervals: [100, 250],
      message,
    })
    .toBe(true);
  return hits(lines, re, from).find((h) => accept(h.m))!;
}

async function setWtScreenQueueOverride(page: Page, bytes: number | null): Promise<void> {
  const accepted = await page.evaluate(
    (b) =>
      (window.__vcNetsim as unknown as WtScreenHook | undefined)?.setWtScreenQueueOverride?.(b),
    bytes,
  );
  expect(accepted, `setWtScreenQueueOverride(${bytes}) was not accepted`).toBe(true);
}

async function pauseShare(page: Page): Promise<void> {
  const paused = await page.evaluate(() => {
    const mock = (window as unknown as ShareMock).__e2e3077;
    if (!mock) return false;
    mock.animate = false;
    return true;
  });
  expect(paused, "the share mock's control object is missing").toBe(true);
}

async function showWhiteSlide(page: Page): Promise<void> {
  const painted = await page.evaluate(() => {
    const mock = (window as unknown as ShareMock).__e2e3077;
    if (!mock) return false;
    mock.showWhiteSlide();
    return true;
  });
  expect(painted, "the share mock's control object is missing").toBe(true);
}

function checksumSum(checksum: string | null): number {
  return checksum === null ? NaN : Number(checksum.split(":")[1]);
}

async function waitForRepaint(page: Page, from: string, message: string): Promise<string> {
  const seen = { value: from };
  await expect
    .poll(
      async () => {
        const sample = await sampleScreenShareChecksum(page);
        if (sample !== null) seen.value = sample;
        return sample !== null && sample !== from;
      },
      { timeout: KEYFRAME_REACH_TIMEOUT_MS, intervals: [250, 500], message },
    )
    .toBe(true);
  return seen.value;
}

async function waitForLiveShare(page: Page, message: string): Promise<void> {
  await expect
    .poll(
      async () =>
        distinctChecksumsInWindow(
          await sampleScreenShareChecksumSeries(page, LIVE_WINDOW_MS, 200),
          0,
          Infinity,
        ),
      { timeout: LIVE_REACH_TIMEOUT_MS, intervals: [0], message },
    )
    .toBeGreaterThanOrEqual(LIVE_MIN_DISTINCT);
}

async function clearOverrideQuietly(page: Page | null): Promise<void> {
  if (!page || page.isClosed()) return;
  let timer: ReturnType<typeof setTimeout> | undefined;
  await Promise.race([
    page.evaluate(() =>
      (window.__vcNetsim as unknown as WtScreenHook | undefined)?.setWtScreenQueueOverride?.(null),
    ),
    new Promise((resolve) => {
      timer = setTimeout(resolve, TEARDOWN_TIMEOUT_MS);
    }),
  ]).catch(() => undefined);
  clearTimeout(timer);
}

test.describe("issue 3077 WT screen send gate driven by the screen-stream queue override", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("override drops screen deltas, then keyframes over the ceiling, and clearing it emits the owed keyframe, also on a static share @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_wt_screen_gate_${Date.now()}`;
    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];
    try {
      const profiles = [
        { email: "wt-screen-gate-sharer@videocall.rs", name: "WtGateSharer" },
        { email: "wt-screen-gate-viewer@videocall.rs", name: "WtGateViewer" },
      ];
      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }
      const sharerCtx = members[0].context;
      await pinWebTransportTransport(sharerCtx);
      await setTransportBadgeFlag(sharerCtx, "true");
      await enableDiagnosticsTileIndicators(sharerCtx);
      await sharerCtx.addInitScript(SHARE_MOCK);

      const sharerLines = collectTimedConsoleFromContext(sharerCtx);
      members[0].page = await joinMeetingAs(sharerCtx, meetingId, profiles[0].name);
      const sharerPage = members[0].page;
      await clickJoinAndEnterGrid(sharerPage);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      const viewerPage = members[1].page;
      await admitGuestIfNeeded(sharerPage, viewerPage);

      await expect(
        sharerPage.locator(SELF_WT_BADGE),
        "the sharer must have ELECTED WebTransport; the override is ignored on WebSocket, so " +
          "a WS sharer would not exercise the #3077 gate.",
      ).toHaveCount(1, { timeout: 30_000 });
      const hookType = await sharerPage.evaluate(
        () =>
          typeof (window.__vcNetsim as unknown as WtScreenHook | undefined)
            ?.setWtScreenQueueOverride,
      );
      expect(
        hookType,
        "window.__vcNetsim.setWtScreenQueueOverride is missing: the UI was built without the " +
          "`netsim` feature (docker/docker-compose.e2e.yaml sets TRUNK_BUILD_FEATURES=netsim) " +
          "or predates #3077.",
      ).toBe("function");

      expect(
        await startScreenShare(sharerPage, viewerPage),
        "the viewer never rendered the received share tile",
      ).toBe(true);
      await expect(viewerPage.locator(RECEIVED_SHARE_CANVAS)).toHaveCount(1, { timeout: 15_000 });
      await expect
        .poll(() => sampleScreenShareChecksum(viewerPage), {
          timeout: 20_000,
          intervals: [250, 500],
          message: "the viewer's received share canvas never painted a frame",
        })
        .not.toBeNull();
      await waitForLiveShare(
        viewerPage,
        "the viewer's share is not live before any override, so later freezes prove nothing",
      );

      const quietFrom = sharerLines.length;
      await sharerPage.waitForTimeout(BASELINE_QUIET_MS);
      expect(
        hits(sharerLines, WT_DELTA_DROP_RE, quietFrom).map((h) => h.m[0]),
        "the WT gate dropped screen deltas before any override, so the real queue already " +
          "exceeds the threshold and the drops below cannot be attributed to the override.",
      ).toEqual([]);

      // Arm 1: over the delta threshold, under the keyframe ceiling.
      const arm1From = sharerLines.length;
      await setWtScreenQueueOverride(sharerPage, DELTA_DROP_OVERRIDE_BYTES);
      const deltaDrop = await waitForHit(
        sharerLines,
        WT_DELTA_DROP_RE,
        arm1From,
        `the WT screen queue is overridden to ${DELTA_DROP_OVERRIDE_BYTES} B but the screen ` +
          "gate never logged a dropped delta, so it does not read the WT screen-stream depth.",
      );
      const threshold = Number(deltaDrop.m[2]);
      expect(Number(deltaDrop.m[1]), "the gate read a depth other than the override").toBe(
        DELTA_DROP_OVERRIDE_BYTES,
      );
      expect(threshold).toBeLessThan(DELTA_DROP_OVERRIDE_BYTES);
      expect(deltaDrop.m[3], "a dropped WT delta must break the GOP").toBe("gop_broken");

      await viewerPage.waitForTimeout(IN_FLIGHT_DRAIN_MS);
      const held = await sampleScreenShareChecksum(viewerPage);
      expect(held, "the viewer lost its received share canvas").not.toBeNull();
      const firstKeyframe = await waitForRepaint(
        viewerPage,
        held!,
        "the viewer never repainted while screen deltas were dropped, so keyframes under the " +
          "ceiling are not passing the gate.",
      );
      await waitForRepaint(
        viewerPage,
        firstKeyframe,
        "the viewer repainted once but not again while deltas were dropped, so keyframes " +
          "under the ceiling stopped passing the gate.",
      );
      const keyframeOnly = await sampleScreenShareChecksumSeries(viewerPage, LIVE_WINDOW_MS, 200);
      expect(
        keyframeOnly.filter((s) => s.checksum !== null).length,
        "the viewer's received share canvas went unsampleable while deltas were dropped",
      ).toBeGreaterThanOrEqual(WINDOW_MIN_SAMPLES);
      expect(
        distinctChecksumsInWindow(keyframeOnly, 0, Infinity),
        "the viewer kept repainting at the capture rate while the gate dropped every delta, so " +
          "a call site still sends the deltas it drops.",
      ).toBeLessThanOrEqual(KEYFRAME_ONLY_MAX_DISTINCT);
      expect(
        hits(sharerLines, WT_KEYFRAME_DROP_RE, arm1From).map((h) => h.m[0]),
        "a keyframe was dropped under the ceiling",
      ).toEqual([]);
      const arm1Debts = hits(sharerLines, WT_DELTA_DROP_RE, arm1From).map((h) => h.m[3]);
      expect(arm1Debts, "with no keyframe dropped, every delta drop leaves a broken GOP").toEqual(
        arm1Debts.map(() => "gop_broken"),
      );

      // Arm 1b: the queue drains while the GOP is broken; only a dropped keyframe is owed.
      const drainFrom = sharerLines.length;
      await setWtScreenQueueOverride(sharerPage, 0);
      await waitForLiveShare(
        viewerPage,
        "the viewer never went live after the queue drained, so a keyframe did not clear the " +
          "broken GOP.",
      );
      expect(
        hits(sharerLines, WT_DRAINED_RE, drainFrom).map((h) => h.m[0]),
        "a broken GOP requested a keyframe on drain",
      ).toEqual([]);
      expect(
        hits(sharerLines, WT_OWED_EMIT_RE, drainFrom).map((h) => h.m[0]),
        "an owed keyframe was emitted although no keyframe was dropped",
      ).toEqual([]);

      // Arm 2: over the keyframe ceiling.
      const arm2From = sharerLines.length;
      await setWtScreenQueueOverride(sharerPage, CEILING_OVERRIDE_BYTES);
      const keyframeDrop = await waitForHit(
        sharerLines,
        WT_KEYFRAME_DROP_RE,
        arm2From,
        `the WT screen queue is overridden to ${CEILING_OVERRIDE_BYTES} B but no screen ` +
          "keyframe was dropped over the ceiling.",
      );
      const ceiling = Number(keyframeDrop.m[2]);
      expect(Number(keyframeDrop.m[1]), "the gate read a depth other than the override").toBe(
        CEILING_OVERRIDE_BYTES,
      );
      expect(ceiling).toBeGreaterThan(DELTA_DROP_OVERRIDE_BYTES);
      expect(ceiling).toBeLessThan(CEILING_OVERRIDE_BYTES);
      await waitForHit(
        sharerLines,
        WT_DELTA_DROP_RE,
        keyframeDrop.index + 1,
        "after a keyframe was dropped over the ceiling, no delta drop reported debt=owed",
        (m) => m[3] === "owed",
      );

      await viewerPage.waitForTimeout(IN_FLIGHT_DRAIN_MS);
      const frozen = await sampleScreenShareChecksumSeries(viewerPage, FREEZE_WINDOW_MS, 250);
      expect(
        frozen.filter((s) => s.checksum !== null).length,
        "the viewer's received share canvas went unsampleable during the freeze window",
      ).toBeGreaterThanOrEqual(8);
      expect(
        distinctChecksumsInWindow(frozen, 0, Infinity),
        "the viewer kept repainting while every screen frame was over the ceiling",
      ).toBe(1);
      expect(
        hits(sharerLines, WT_DRAINED_RE, arm2From).map((h) => h.m[0]),
        "the owed keyframe was requested while the queue was still over the threshold",
      ).toEqual([]);

      // Arm 3: clear right after a dropped keyframe, so the next chunk is a delta.
      const nextKeyframeDrop = sharerPage.waitForEvent("console", {
        predicate: (msg) => WT_KEYFRAME_DROP_RE.test(msg.text()),
        timeout: GATE_LINE_TIMEOUT_MS,
      });
      await nextKeyframeDrop;
      const arm2Debts = hits(sharerLines, WT_DELTA_DROP_RE, keyframeDrop.index + 1).map(
        (h) => h.m[3],
      );
      const arm3From = sharerLines.length;
      await setWtScreenQueueOverride(sharerPage, null);
      expect(arm2Debts, "over the ceiling, every delta drop must keep the keyframe owed").toEqual(
        arm2Debts.map(() => "owed"),
      );
      const drained = await waitForHit(
        sharerLines,
        WT_DRAINED_RE,
        arm3From,
        "the override was cleared but the gate never requested the owed keyframe.",
      );
      expect(Number(drained.m[2]), "the drained line reports a different gate").toBe(threshold);
      expect(Number(drained.m[1])).toBeLessThanOrEqual(threshold);
      await waitForHit(
        sharerLines,
        WT_OWED_EMIT_RE,
        drained.index + 1,
        "the owed keyframe was requested but the encode loop never emitted it.",
      );

      await waitForLiveShare(
        viewerPage,
        "the viewer never went live after the owed keyframe was emitted, so the debt did not " +
          "clear and deltas are still dropped.",
      );
      expect(
        hits(sharerLines, WT_KEYFRAME_DROP_RE, arm3From).map((h) => h.m[0]),
        "a keyframe was dropped after the override was cleared",
      ).toEqual([]);

      // Arm 4: the content changes once on a static share while every keyframe is over the
      // ceiling; once the floor budget is spent, only the static owed keyframe can deliver it.
      await pauseShare(sharerPage);
      await sharerPage.waitForTimeout(IN_FLIGHT_DRAIN_MS);
      await setWtScreenQueueOverride(sharerPage, CEILING_OVERRIDE_BYTES);
      await viewerPage.waitForTimeout(IN_FLIGHT_DRAIN_MS);
      const previousSlide = await sampleScreenShareChecksum(viewerPage);
      expect(
        checksumSum(previousSlide),
        "the viewer must show a non-white slide before the white one is shared",
      ).toBeLessThan(WHITE_SLIDE_MIN_SUM);
      const arm4From = sharerLines.length;
      await showWhiteSlide(sharerPage);
      await expect
        .poll(
          () => {
            const drops = hits(sharerLines, WT_KEYFRAME_DROP_RE, arm4From);
            if (drops.length < FLOOR_BUDGET) return false;
            const lastAt = sharerLines[drops[drops.length - 1].index].at;
            return Date.now() - lastAt >= FLOOR_SPENT_QUIET_MS;
          },
          {
            timeout: FLOOR_SPENT_TIMEOUT_MS,
            intervals: [250, 500],
            message: `the static share's ${FLOOR_BUDGET} floor keyframes were not dropped over the ceiling`,
          },
        )
        .toBe(true);
      const floorDrops = hits(sharerLines, WT_KEYFRAME_DROP_RE, arm4From).map((h) => h.m[1]);
      expect(floorDrops, "a floor keyframe was judged against another depth").toEqual(
        floorDrops.map(() => String(CEILING_OVERRIDE_BYTES)),
      );
      expect(
        await sampleScreenShareChecksum(viewerPage),
        "a frame reached the viewer while every keyframe was over the ceiling",
      ).toBe(previousSlide);
      expect(
        hits(sharerLines, WT_STATIC_OWED_RE, arm4From).map((h) => h.m[0]),
        "the static share paid its debt while the queue was still over the threshold",
      ).toEqual([]);

      const staticDrainFrom = sharerLines.length;
      await setWtScreenQueueOverride(sharerPage, 0);
      await expect
        .poll(async () => checksumSum(await sampleScreenShareChecksum(viewerPage)), {
          timeout: KEYFRAME_REACH_TIMEOUT_MS,
          intervals: [250, 500],
          message:
            "the queue drained but the viewer never showed the white slide, so it keeps the " +
            "previous slide until the content changes again.",
        })
        .toBeGreaterThanOrEqual(WHITE_SLIDE_MIN_SUM);
      const staticPaid = await waitForHit(
        sharerLines,
        WT_STATIC_OWED_RE,
        staticDrainFrom,
        "the white slide arrived without the static share's owed-keyframe re-encode",
      );
      expect(
        [Number(staticPaid.m[1]), Number(staticPaid.m[2])],
        "the static re-encode read another depth or gate",
      ).toEqual([0, threshold]);
      await expect(sharerPage.locator(SELF_WT_BADGE)).toHaveCount(1, { timeout: 5_000 });
    } finally {
      await clearOverrideQuietly(members[0]?.page ?? null);
      for (const m of members) {
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
