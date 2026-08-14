import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Issue 2174 follow-up — a mute must EXTINGUISH the speaking glow and keep it
 * out.
 *
 * WHAT REGRESSED (the user-visible defect this file pins)
 * ------------------------------------------------------
 * The remote speaking glow has a fast path that does not go through the
 * heartbeat: `NetEqAudioPeerDecoder::handle_pcm_data` runs a VAD over the
 * DECODED PCM and broadcasts `peer_speaking` on the diagnostics bus, which
 * `PeerTile::handle_diagnostics_event` turns into the tile's `audio_level`
 * signal and, through `speak_style()`, into the tile's inline `box-shadow` /
 * `border-color` / `transition`.
 *
 * When a peer is muted (by themselves, or by the host) the receiving client
 * calls `set_muted(true)` on that peer's audio decoder, which broadcasts a
 * terminal `speaking: 0` and darkens the tile. But the NetEQ worker produces
 * PCM on a 10ms timer (`neteq/src/bin/neteq_worker.rs`) and only learns about
 * the mute when it dispatches the `Mute` message the main thread posted. Every
 * PCM frame produced in that gap — plus anything already sitting in the main
 * thread's message queue — is dispatched to `handle_pcm_data` AFTER the
 * terminal zero, still carrying real speech. Before the fix that frame saw
 * `speaking == false`, treated itself as a fresh rising edge, and broadcast
 * `speaking: 1` with a real positive level, RE-LIGHTING the glow on a tile
 * whose mic glyph already read muted. It stayed lit until the peer's next
 * heartbeat (up to `HEARTBEAT_KEEPALIVE_INTERVAL_MS` = 5s) or, absent
 * heartbeats, until the 12.5s glow deadman.
 *
 * WHY THE EXISTING SPECS DO NOT COVER THIS
 * ----------------------------------------
 * `speaker-highlight.spec.ts:437` already drives this exact flow — join, speak,
 * mute, poll for the silent style — but its poll budget is 30_000ms (its roster
 * sibling at :1582 is 15_000ms). The pre-fix bug SELF-CLEARS at the ≤5s
 * keepalive, so both of those pass on fixed AND un-fixed code. A spec for this
 * defect is only a discriminator if its budget sits strictly under 5000ms; see
 * [`GLOW_OUT_BUDGET_MS`] for the arithmetic this file uses.
 *
 * WHAT THIS SPEC DISCRIMINATES (and what it does not)
 * --------------------------------------------------
 * Two independent guards now stand between that stale frame and the tile:
 *   1. `VadState::suppressed` in `neteq_audio_decoder.rs` — the mute path shuts
 *      the VAD gate before broadcasting the terminal zero, so the late frame is
 *      dropped at the SOURCE.
 *   2. `speaking_event_resolution` in `peer_tile.rs` — the `peer_speaking` arm
 *      now applies the same `audio_enabled` veto as the `peer_status` arm, so a
 *      stale claim is dropped at the SINK.
 * A browser can only observe the rendered result, so these tests fail when BOTH
 * guards are absent (i.e. against the un-fixed build) and pass when either one
 * is present. They are a regression test for the shipped defect, NOT a
 * per-guard discriminator — the single-guard mutation sensitivity is pinned by
 * the Rust unit tests (`a_straggler_speaking_event_cannot_relight_a_muted_peer`,
 * `speaking_peer_yields_one_terminal_event_and_resets`).
 *
 * The SELF-MUTE test is the one that carries the discriminating weight, because
 * the mute reaches the observer via the peer's own heartbeat and the next
 * correcting heartbeat is a full keepalive (5s) away. On the HOST-FORCE-MUTE
 * path the target's own edge-triggered heartbeat arrives independently, ~100ms
 * to a few hundred ms behind the NATS command, and clears a pre-fix re-light on
 * its own — so that test asserts the same correct behaviour but with far less
 * pre-fix margin. Read it as coverage of the `force_peer_media_off` call site,
 * not as the receipt.
 *
 * WHAT IS DELIBERATELY NOT ASSERTED HERE: the roster (peer-list) half of the
 * same fix. `peer_list_item.rs` renders the roster mic icon as
 * `if speaking && !muted`, and `muted` is derived from the very same
 * `peer_audio_states` map that `resolve_roster_speaking` consults — so a
 * straggler that flips the stored `speaking` flag can never light the roster
 * icon while the peer reads muted. That guard is real defense-in-depth but has
 * no DOM-observable effect in this window, and a Playwright assertion on it
 * would pass identically on both sides. Its coverage is the Rust unit test
 * `a_muted_peer_cannot_light_the_roster_speaking_dot`.
 *
 * TAGGING: deliberately UNTAGGED (no `@bvt0` / `@bvt1`). Each test drives two
 * real browsers through a full join dance with a fake microphone and then holds
 * an 8s observation window, so it costs ~90s — far outside the "~tens of
 * seconds" budget `playwright.config.ts` documents for the bvt1 smoke superset.
 * It therefore does NOT run in per-PR CI and must be validated by a full
 * `--project=dioxus` run (local `make e2e`, or a scoped `/run-e2e dioxus`
 * dispatch) before the change it guards is considered covered.
 */

/** Chromium args, optionally swapping the fake mic for a WAV fixture. */
function browserArgs(fakeAudioFile?: string): string[] {
  if (!fakeAudioFile) {
    return [...BROWSER_ARGS];
  }
  return [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${fakeAudioFile}`];
}

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
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
  await page.waitForTimeout(1500);
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting" || result === "waiting-for-meeting") {
    return result;
  }
  if (result === "auto-joined") {
    return "in-meeting";
  }

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult !== "waiting") {
    return;
  }

  const admitButton = hostPage.getByTitle("Admit").first();
  await expect(admitButton).toBeVisible({ timeout: 20_000 });
  await hostPage.waitForTimeout(1000);
  await admitButton.dispatchEvent("click");
  await hostPage.waitForTimeout(3000);

  const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
  const guestGrid = guestPage.locator("#grid-container");
  const postAdmit = await Promise.race([
    guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
    guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
  ]);

  if (postAdmit === "join-button") {
    await guestPage.waitForTimeout(1000);
    await guestJoinButton.click();
    await guestPage.waitForTimeout(3000);
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

interface TwoUserMeeting {
  hostPage: Page;
  guestPage: Page;
  browser1: Browser;
  browser2: Browser;
}

/**
 * Host + guest in one meeting, with the guest's fake microphone driven by the
 * swept-amplitude tone fixture so the decoder-side VAD reports the guest as
 * CONTINUOUSLY speaking (see `helpers/audio-fixtures.ts` for why a constant
 * tone would not work: the fast path is edge-triggered and would go silent).
 */
async function setupSpeakingMeeting(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
): Promise<TwoUserMeeting> {
  const browser1 = await chromium.launch({ args: browserArgs() });
  const browser2 = await chromium.launch({ args: browserArgs(continuousToneWavPath()) });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    `${hostName.toLowerCase()}@videocall.rs`,
    hostName,
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    `${guestName.toLowerCase()}@videocall.rs`,
    guestName,
    uiURL,
  );

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, hostName);
  expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  await admitGuestIfNeeded(hostPage, guestPage, await joinMeetingFromPage(guestPage));

  await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
  await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 45_000,
  });

  return { hostPage, guestPage, browser1, browser2 };
}

/**
 * The dock's mic toggle.
 *
 * Selected by the stable `data-testid` the button carries for exactly this
 * purpose rather than by tooltip text: the tooltip now reads
 * "Microphone — Mute" / "Microphone — Unmute" (`video_control_buttons.rs`), and
 * Playwright's `hasText` is a case-insensitive substring match, so a `"Mute"`
 * filter also matches the UNMUTE button. The button's own class carries the
 * state — `active` when the mic is on, `off` when it is muted.
 */
function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

/** Turn the local microphone on. The mic starts muted in the E2E stack. */
async function enableMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

/** The dock's mic toggle, confirmed to be in the UNMUTED state and clickable. */
async function armedMicMuteButton(page: Page): Promise<Locator> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
  return toggle;
}

/**
 * Open the host-only per-tile context menu on the remote peer's tile and return
 * the "Mute" item, ready to click.
 *
 * Anchored to `canvas_generator.rs`: the toggle is `button.tile-mute-btn` with
 * `title="Host actions"`, the items are `.tile-context-menu-item`. The toggle
 * is `visibility: hidden` until `.grid-item:hover`, hence the hover. Scoping to
 * `.grid-item:has(.tile-mute-btn)` skips any tile without host actions.
 */
async function openHostMuteMenuItem(page: Page) {
  const guestTile = page.locator(".grid-item:has(.tile-mute-btn)").first();
  await expect(guestTile).toBeVisible({ timeout: 30_000 });
  await guestTile.hover();

  const muteToggle = guestTile.getByTitle("Host actions");
  await expect(muteToggle).toBeVisible({ timeout: 15_000 });
  await muteToggle.click();

  const muteMenuItem = guestTile.locator(".tile-context-menu-item", { hasText: "Mute" });
  await expect(muteMenuItem).toBeVisible({ timeout: 5_000 });
  return muteMenuItem;
}

// ---------------------------------------------------------------------------
// Glow timeline
// ---------------------------------------------------------------------------

type GlowVerdict = "lit" | "silent" | "unknown";

/** One recorded state of the tracked tile, or an explicit phase marker. */
interface GlowSample {
  at: number;
  style: string;
  cls: string;
  /**
   * The tile's own `data-mic-muted` at capture time — `"true"` once THIS
   * client's decode manager reports the peer as audio-off
   * (`is_audio_enabled_for_peer` in `canvas_generator.rs`). `null` on the
   * synthetic `oldValue` entries, which carry a past style rather than a state
   * read. This is the zero point the deadline is measured from; it is read at
   * 50ms resolution from inside the page rather than through a Playwright poll,
   * because a Playwright poll can only observe the flip AFTER the fact and
   * would move the zero point past the very window under test.
   */
  muted: string | null;
  missing: boolean;
  marker: string | null;
}

interface GlowWindow {
  __vcGlowSamples?: GlowSample[];
  __vcGlowMark?: (label: string) => void;
  __vcGlowStop?: () => void;
}

/**
 * Classify a tile's inline style as `speak_style`'s GLOWING or SILENT output.
 *
 * Keyed on the transition EASING, which is the one property the two branches
 * never share (`canvas_generator.rs::speak_style`):
 *   - both silent branches emit `... ease-out ...` for border-colour AND
 *     box-shadow, and never `ease-in`;
 *   - both glowing branches emit `... ease-in ...` for both, and never
 *     `ease-out`.
 * Colour literals are unusable (themed, and already drifted once), and
 * `box-shadow: none` is emitted by a GLOWING tile too when
 * `inner_glow_strength` is 0, so neither can stand alone.
 *
 * Anything that is neither — an empty attribute, a detached node, a future
 * style this classifier does not understand — is `"unknown"` and NEVER silently
 * folded into one of the two verdicts. A predicate shaped as "silent = <two
 * substrings>" scores an empty string as glowing, which turns a missing
 * attribute into evidence, in whichever direction the caller happens to assert.
 * The callers below assert that no `"unknown"` appears in the measured window.
 */
function classifyGlow(style: string): GlowVerdict {
  const lit = style.includes("ease-in");
  const silent = style.includes("ease-out");
  if (lit === silent) {
    return "unknown";
  }
  return lit ? "lit" : "silent";
}

/**
 * Start recording every rendered state of the tile with id `tileId`.
 *
 * Two capture paths run together because the defect can be shorter than a poll
 * interval. The MutationObserver records each style mutation's `oldValue`, so a
 * glow that is switched on and back off inside one microtask batch is still
 * preserved (re-reading the DOM from the callback would see only the final
 * value). The 50ms interval re-queries the id from scratch, so it keeps
 * reporting if Dioxus rebuilds the tile outside the observed subtree, and it
 * records the "tile is missing" case that would make a clean result vacuous.
 *
 * THROWS if `#grid-container` is absent: falling back to interval-only sampling
 * would still collect enough samples to satisfy every non-vacuity check while
 * quietly losing the sub-poll capture, so the degraded mode must be an error.
 */
async function startGlowTimeline(page: Page, tileId: string): Promise<void> {
  await page.evaluate((id) => {
    const w = window as unknown as GlowWindow;
    const samples: GlowSample[] = [];
    w.__vcGlowSamples = samples;

    const sample = (marker: string | null) => {
      const el = document.getElementById(id);
      samples.push({
        at: performance.now(),
        style: el?.getAttribute("style") || "",
        cls: el?.getAttribute("class") || "",
        muted: el?.querySelector("[data-mic-muted]")?.getAttribute("data-mic-muted") ?? null,
        missing: el === null,
        marker,
      });
    };

    sample(null);

    const container = document.getElementById("grid-container");
    if (!container) {
      throw new Error(
        "#grid-container not found — cannot install the glow MutationObserver, so a sub-poll " +
          "re-light would go unrecorded and a pass would be meaningless",
      );
    }

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.attributeName === "style" && (record.target as Element).id === id) {
          // The value the tile held BEFORE this mutation. Pushed ahead of the
          // post-mutation sample below, so the array stays in chronological
          // order.
          samples.push({
            at: performance.now(),
            style: record.oldValue || "",
            cls: "",
            muted: null,
            missing: false,
            marker: null,
          });
        }
      }
      sample(null);
    });
    observer.observe(container, {
      subtree: true,
      attributes: true,
      attributeOldValue: true,
      // `data-mic-muted` is watched too so the mute flip itself schedules a
      // sample, pinning the deadline's zero point to the render that carried it
      // rather than to the next 50ms tick.
      attributeFilter: ["style", "class", "data-mic-muted"],
    });

    const timer = window.setInterval(() => sample(null), 50);

    w.__vcGlowMark = (label: string) => sample(label);
    w.__vcGlowStop = () => {
      observer.disconnect();
      window.clearInterval(timer);
    };
  }, tileId);
}

/**
 * Write a phase marker into the timeline.
 *
 * The regression assertion needs an unambiguous "the mute was issued HERE"
 * boundary. Inferring it from the samples themselves (e.g. "the first silent
 * one") would beg the question the test is asking.
 */
async function markGlowTimeline(page: Page, label: string): Promise<void> {
  await page.evaluate((l) => {
    const w = window as unknown as GlowWindow;
    if (!w.__vcGlowMark) {
      throw new Error(`glow timeline not running — cannot mark "${l}"`);
    }
    w.__vcGlowMark(l);
  }, label);
}

async function stopGlowTimeline(page: Page): Promise<GlowSample[]> {
  return page.evaluate(() => {
    const w = window as unknown as GlowWindow;
    w.__vcGlowStop?.();
    return w.__vcGlowSamples ?? [];
  });
}

function describeSample(s: GlowSample | undefined, zero = 0): string {
  if (!s) {
    return "n/a";
  }
  return `at=+${(s.at - zero).toFixed(0)}ms verdict=${classifyGlow(s.style)} style="${s.style}"`;
}

/**
 * How long after the mute is APPLIED the glow is still allowed to be lit.
 *
 * THE ARITHMETIC (this is the whole discriminator — see the "why a deadline"
 * note below):
 *
 *   Fixed code: `set_muted(true)` broadcasts the terminal `speaking: 0`
 *   SYNCHRONOUSLY, in the same turn that sets `Peer::audio_enabled = false` —
 *   the very state `data-mic-muted` renders. The tile drains that event off the
 *   `videocall_diagnostics` bus in a `spawn_local` loop and Dioxus commits, so
 *   the glow is out within a frame or two of the flip: single-digit to low-tens
 *   of milliseconds. 1000ms is ~2 orders of magnitude of headroom.
 *
 *   Pre-fix code: the straggler re-lights the glow immediately after that same
 *   terminal zero, and NOTHING corrects it until the peer's next keepalive
 *   heartbeat — `HEARTBEAT_KEEPALIVE_INTERVAL_MS` = 5000 (videocall-aq/src/
 *   constants.rs) — whose `is_speaking` is producer-gated to 0 and whose
 *   `audio_enabled = 0` drives `effective_level` to 0.0. (The 12.5s glow
 *   deadman is the later backstop, and the straggler refreshes it anyway.)
 *
 * So the budget must sit strictly between "a couple of render frames" and
 * "5000ms". 1000ms is 5x under the keepalive and 100x over the render path.
 *
 * Being 1000ms — not 13s — the measured window is also nowhere near the 12.5s
 * deadman, so the deadman can neither rescue a broken build nor fail a correct
 * one inside it.
 */
const GLOW_OUT_BUDGET_MS = 1_000;

/**
 * The verdict: once THIS client knows the peer is muted, the speaking glow must
 * be out within [`GLOW_OUT_BUDGET_MS`] and stay out for the rest of the window.
 *
 * WHY A DEADLINE, and not "it went dark, then came back on". The obvious shape
 * for this defect is the dark→lit→dark blink, but asserting that shape is
 * VACUOUS against the pre-fix build in the case that matters most: the terminal
 * zero and the straggler arrive on the bus one dispatch turn apart, so there is
 * no guarantee the browser ever COMMITS a silent frame between them. If it does
 * not, "the first silent sample" is the one produced by the 5s keepalive, every
 * sample after it is silent, and a blink assertion passes on broken code. A
 * deadline measured from the mute flip has no such hole: the pre-fix glow is
 * lit for ~5s past that point however the intermediate frames land.
 *
 * The window (mute-applied, mute-applied + budget] is deliberately NOT policed.
 * The client's `audio_enabled` field flips synchronously while the tile's own
 * signals are still catching up off the bus, so an unrelated re-render landing
 * in that gap can legitimately paint a muted glyph beside a not-yet-zeroed
 * glow. Policing it would buy nothing — the pre-fix defect persists for 5x the
 * budget — and would risk a false red on correct code.
 */
function assertGlowGoesOutWithinBudget(samples: GlowSample[]): void {
  const issuedAt = samples.findIndex((s) => s.marker === "mute-issued");
  expect(issuedAt, "the timeline must carry a mute-issued marker").toBeGreaterThanOrEqual(0);

  // Non-vacuity: the recorder observed a live tile for the whole window.
  expect(samples.length, "too few samples — the recorder was not running").toBeGreaterThan(80);
  expect(
    samples.filter((s) => s.missing).length,
    "the tracked tile disappeared mid-window — the recording is not meaningful",
  ).toBe(0);

  // Non-vacuity: the guest really was glowing before the mute, so "goes dark"
  // is not trivially true for a tile that never lit at all.
  const preMuteLit = samples.slice(0, issuedAt).filter((s) => classifyGlow(s.style) === "lit");
  expect(
    preMuteLit.length,
    "no glowing sample was recorded before the mute — the peer never registered as speaking, " +
      "so this run proves nothing about the mute path",
  ).toBeGreaterThan(2);

  // The zero point: the first render in which THIS client reports the peer as
  // audio-off. Taken from the timeline (50ms resolution, plus a sample forced by
  // the attribute mutation itself), never from a Playwright poll.
  const applied = samples.slice(issuedAt).find((s) => s.muted === "true");
  expect(
    applied,
    "this client never rendered the peer as muted — the mute did not reach its decode manager, " +
      "so there is nothing to measure",
  ).toBeTruthy();
  const zero = (applied as GlowSample).at;
  const deadline = zero + GLOW_OUT_BUDGET_MS;

  // Non-vacuity: the recording must actually extend past the deadline, or
  // "nothing lit after the deadline" is true because nothing was recorded there.
  const measured = samples.filter((s) => s.at > deadline);
  expect(
    measured.length,
    `the recording ended at +${(samples[samples.length - 1].at - zero).toFixed(0)}ms, before the ` +
      `+${GLOW_OUT_BUDGET_MS}ms deadline — the observation window is too short to conclude anything`,
  ).toBeGreaterThan(20);

  // Classifier hygiene: every style in the measured window must be one this spec
  // understands, so a `speak_style` rewrite cannot quietly turn the assertion
  // below into a no-op.
  const unknown = measured.filter((s) => classifyGlow(s.style) === "unknown");
  expect(
    unknown.length,
    `${unknown.length} sample(s) past the deadline carried a style this spec cannot classify as ` +
      `speak_style output — first: ${describeSample(unknown[0], zero)}`,
  ).toBe(0);

  // The regression assertion.
  const stillLit = measured.filter((s) => classifyGlow(s.style) === "lit");
  expect(
    stillLit.length,
    `the speaking glow was still lit in ${stillLit.length}/${measured.length} samples more than ` +
      `${GLOW_OUT_BUDGET_MS}ms after this client rendered the peer as muted (issue 2174 ` +
      `follow-up: a straggler peer_speaking event, carrying PCM the NetEQ worker had already ` +
      `posted, re-lit a muted peer and nothing corrected it until the 5s keepalive). ` +
      `First late-lit sample: ${describeSample(stillLit[0], zero)}`,
  ).toBe(0);
}

/**
 * Bring the host's view of the guest to a settled, glowing state and start
 * recording it. Returns the tile id and the tile locator.
 */
async function beginGlowObservation(hostPage: Page): Promise<{ tileId: string; tile: Locator }> {
  // The local user's own self-view is the `Host` component (`#host-controls-nav`,
  // class `.host`), NOT a `.grid-item` — so in a two-user meeting the grid holds
  // exactly the guest's tile. Assert that rather than reaching for `.first()`
  // and hoping: if a future layout ever puts the self-view in the grid, this
  // fails loudly instead of silently tracking the wrong tile.
  const tiles = hostPage.locator("#grid-container .grid-item");
  await expect(tiles.first()).toBeVisible({ timeout: 30_000 });
  await expect(tiles, "expected exactly one remote tile (the guest) in the grid").toHaveCount(1);
  const tile = tiles.first();

  const tileId = await tile.getAttribute("id");
  expect(tileId, "the guest tile needs a stable id to track across renders").toBeTruthy();

  // Presence before measurement: the tile must be a live, UNMUTED peer.
  // `data-mic-muted` is read as a value rather than through `toBeVisible` so the
  // assertion is about the state the tile reports, not about CSS.
  await expect(tile.locator("[data-mic-muted]").first()).toHaveAttribute(
    "data-mic-muted",
    "false",
    { timeout: 30_000 },
  );

  // ...and it must be glowing, twice over — once to catch the transition and
  // once to confirm it is settled rather than a transient caught on its way
  // out. Kept short so the whole lit phase stays well inside the 12.5s glow
  // deadman, which would otherwise put the glow out for a reason unrelated to
  // the mute.
  await expect
    .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
      timeout: 45_000,
      message: "expected the guest tile to enter the speaking-glow state",
    })
    .toBe("lit");
  await hostPage.waitForTimeout(1_000);
  await expect
    .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
      timeout: 10_000,
      message: "expected a settled glow on the tile before recording",
    })
    .toBe("lit");

  await startGlowTimeline(hostPage, tileId as string);
  // Record a lit baseline before the caller issues the mute. Without this the
  // pre-mute slice would be however many samples the caller's own setup calls
  // happened to take, which is not something the test should depend on.
  await hostPage.waitForTimeout(1_500);
  return { tileId: tileId as string, tile };
}

/** Watch the recorded window run past the point where a straggler could land. */
async function finishGlowObservation(hostPage: Page, tile: Locator): Promise<void> {
  // Wait for the muted glyph on the host's own render of the guest tile — the
  // receipt that the mute reached THIS client's decode manager
  // (`is_audio_enabled_for_peer` in `canvas_generator.rs`). This is only a WAIT,
  // not the measurement zero point: on the self-mute path the flip can take up
  // to ~5s (the off-heartbeat is overridden while audio frames are still fresh
  // — LIVE_STREAM_FRESH_WINDOW_MS = 500 — and the next evaluation is the 5s
  // keepalive), and a Playwright poll can only see it after the fact. The
  // deadline is measured from the flip as the in-page recorder saw it.
  await expect(tile.locator("[data-mic-muted]").first()).toHaveAttribute("data-mic-muted", "true", {
    timeout: 30_000,
  });

  // Run 8s past the flip: comfortably beyond the +1000ms deadline, and past the
  // 5s keepalive that would clear a pre-fix re-light — so a failure reads as a
  // sustained lit glow rather than an ambiguous edge.
  await hostPage.waitForTimeout(8_000);

  const samples = await stopGlowTimeline(hostPage);
  assertGlowGoesOutWithinBudget(samples);

  // Final state, read straight from the DOM rather than the recording.
  await expect(tile).not.toHaveClass(/speaking-tile/);
  expect(classifyGlow((await tile.getAttribute("style")) || "")).toBe("silent");
}

test.describe("Speaking glow must stay out after a mute", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * HOST FORCE-MUTE. The host's own client receives the HOST_MUTE_PARTICIPANT
   * broadcast it just sent and applies `force_peer_media_off(target, true,
   * false)` (`video_call_client.rs:4986`), which calls `set_muted(true)` +
   * `flush()` on the guest's audio decoder and bypasses the heartbeat freshness
   * window. That is the authoritative, sub-second mute path.
   *
   * PRE-FIX MARGIN IS SMALL HERE, by design of the feature rather than of the
   * test: the guest also mutes locally on receiving the same command and sends
   * an edge-triggered heartbeat, which reaches the host ~100ms to a few hundred
   * ms later carrying `audio_enabled = 0` and clears a pre-fix re-light on its
   * own — possibly inside the +1000ms deadline. So this test reliably pins the
   * CORRECT behaviour of the `force_peer_media_off` call site, but it is the
   * self-mute test below, not this one, that is the discriminating receipt.
   */
  test("a host-force-muted peer's tile glow does not come back on", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_hostmute_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupSpeakingMeeting(
      uiURL,
      meetingId,
      "GlowMuteHost",
      "GlowMuteGuest",
    );

    try {
      // The tile's host-actions menu only renders while the host sees the peer
      // as audio-enabled, so the guest must be unmuted first.
      await enableMic(guestPage);

      const { tile } = await beginGlowObservation(hostPage);

      // Open the menu BEFORE marking, so the marker sits immediately before the
      // click that issues the mute rather than a second of menu navigation
      // earlier.
      const muteMenuItem = await openHostMuteMenuItem(hostPage);
      await markGlowTimeline(hostPage, "mute-issued");
      await muteMenuItem.click();

      await finishGlowObservation(hostPage, tile);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  /**
   * SELF-MUTE — THE DISCRIMINATOR. The guest mutes their own microphone; the
   * host learns about it from the guest's heartbeat and applies the audio-off
   * transition through `apply_live_stream_heartbeat_flag`, reaching the same
   * `set_muted(true)` + `flush()` pair from a different call site.
   *
   * This is the path with the full pre-fix margin. The heartbeat that applies
   * the mute is the SAME event that would otherwise have corrected a re-lit
   * glow, so on un-fixed code the next correction is a whole
   * HEARTBEAT_KEEPALIVE_INTERVAL_MS (5000ms) away — 5x the +1000ms deadline.
   * It is also the common shape of the reported defect: users mute themselves
   * far more often than a host force-mutes them.
   */
  test("a self-muted peer's tile glow does not come back on", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_glow_selfmute_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupSpeakingMeeting(
      uiURL,
      meetingId,
      "GlowSelfHost",
      "GlowSelfGuest",
    );

    try {
      await enableMic(guestPage);

      const { tile } = await beginGlowObservation(hostPage);

      const muteBtn = await armedMicMuteButton(guestPage);
      await markGlowTimeline(hostPage, "mute-issued");
      await muteBtn.click();
      await expect(muteBtn).toHaveClass(/\boff\b/, { timeout: 15_000 });

      await finishGlowObservation(hostPage, tile);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
