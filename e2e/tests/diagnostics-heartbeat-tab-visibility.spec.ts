import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Diagnostics heartbeat — tab-visibility throttling fix.
 *
 * Background
 * ----------
 * Chrome throttles main-thread `setInterval` / `setTimeout` callbacks to a
 * minimum of ~1000ms when the tab is hidden. The receive-side diagnostics
 * managers (`videocall_client::diagnostics::diagnostics_manager`) used to set
 * up their 500ms heartbeat via `window.setInterval`, which meant the
 * adaptive-quality (AQ) feedback loop and per-peer reporting both got cut to
 * 1Hz the moment the user backgrounded the meeting tab. This is the most
 * commonly reported root cause for "audio/video quality degrades when I tab
 * away from the meeting".
 *
 * The fix moves the heartbeat onto a `DedicatedWorkerGlobalScope` Worker
 * spawned from an inline blob: URL, because Worker-scope timers are NOT
 * subject to background-tab throttling. See
 * `videocall-client/src/diagnostics/heartbeat.rs` for the full design notes.
 *
 * What this test covers
 * ---------------------
 * Playwright cannot directly reproduce Chrome's renderer-side visibility
 * throttling. That mechanism lives below the JS layer: it keys off the
 * *browser* backgrounding the tab (occluded / minimised / non-foreground),
 * NOT off the value `document.hidden` reports. Overriding the getter and
 * dispatching `visibilitychange` makes the page *report* hidden without
 * putting the renderer into the throttled state. So no test in this file can
 * prove "ticks survive real background throttling" — that remains a
 * manual-only verification (steps are in the PR #707 body).
 *
 * What we CAN verify here, and do:
 *
 *   1. The Worker-backed heartbeat is spawned (asserted via the
 *      `diagnostics heartbeat: spawned worker` log line that
 *      `heartbeat.rs::start_worker` emits at `info!` level), and the fallback
 *      log line ("worker unavailable") is NOT present — i.e. we did not
 *      silently degrade to the throttled main-thread path.
 *
 *   2. Real heartbeat ticks keep being delivered to the production
 *      `onmessage` consumer across a `visibilitychange` to hidden, at the
 *      cadence `HEARTBEAT_PERIOD_MS` configures (issue #720). This is a
 *      strictly weaker claim than "survives renderer throttling", and the
 *      test name says exactly that: *while the page reports hidden*. Its
 *      value is that it is a real observation of the tick pipeline rather
 *      than a restatement of the test's own override — see the observer
 *      contract below.
 *
 * What's deliberately NOT covered (manual-test only):
 *
 *   - The actual Chrome renderer throttling behavior. Verifying this requires
 *     a real OS tab switch with the user agent actually backgrounded. See
 *     the PR body for the manual repro steps.
 *   - Safari / Firefox background-throttling parity. Different browsers have
 *     different rules; this fix only addresses the Chromium case.
 *   - The follow-up "audio-only when hidden" optimisation, which is tracked
 *     as a separate issue.
 */

declare global {
  interface Window {
    /**
     * Test-only heartbeat observer installed by `HEARTBEAT_OBSERVER_SCRIPT`
     * below. One entry per `new Worker(...)` the page constructs, in
     * construction order.
     */
    __vcHeartbeatObserver?: {
      /** `true` once the `onmessage` interception is wired up. */
      installed: boolean;
      /** Every Worker the page built: its script URL and its tick count. */
      workers: Array<{ url: string; ticks: number }>;
    };
  }
}

/**
 * The production heartbeat cadence.
 *
 * Source of truth: `videocall-client/src/diagnostics/diagnostics_manager.rs:42`
 *   `const HEARTBEAT_PERIOD_MS: u32 = 500;`
 * passed to `HeartbeatTimer::start(HEARTBEAT_PERIOD_MS, ..)` at both call
 * sites (`:250` receive-side, `:706` send-side), and forwarded to the Worker
 * as `periodMs` (`heartbeat.rs:263-272`), which the Worker script uses as its
 * `setInterval` period (`heartbeat.rs:107-110`).
 *
 * Every wait below is DERIVED from this constant — no magic sleeps.
 */
const HEARTBEAT_PERIOD_MS = 500;

/**
 * How many ticks a single heartbeat Worker must deliver AFTER the visibility
 * flip before we call the heartbeat "still running". Six ticks is three full
 * seconds of cadence — comfortably more than the 1Hz clamp a throttled
 * main-thread timer would impose, so the assertion is measuring a real
 * stream of ticks and not one straggler.
 */
const MIN_TICKS_AFTER_HIDDEN = 6;

/**
 * Ticks required BEFORE the flip, as a presence gate. Asserting this first is
 * what stops the measurement below from passing vacuously if the observer
 * never attaches (a never-populated counter would otherwise sit at 0 and a
 * delta assertion would be measuring nothing).
 */
const MIN_TICKS_BEFORE_HIDDEN = 2;

/**
 * Wall-clock budget for collecting N ticks at `HEARTBEAT_PERIOD_MS`. 4x the
 * theoretical minimum: generous enough to absorb wasm main-thread contention
 * and the 2-worker Playwright run, tight enough that a stalled heartbeat
 * fails in seconds rather than hanging to the suite timeout.
 */
const tickBudgetMs = (ticks: number) => ticks * HEARTBEAT_PERIOD_MS * 4;

/**
 * Marker strings from the production Worker script, used to prove the Worker
 * we counted ticks from is the heartbeat one and not some other Worker the
 * app happens to run (decoder / neteq).
 *
 * Source: `videocall-client/src/diagnostics/heartbeat.rs:99-118`
 *   (`HEARTBEAT_WORKER_JS` — `setInterval(() => { self.postMessage({ type: 'tick' }) }, periodMs)`)
 */
const HEARTBEAT_WORKER_JS_MARKERS = ["self.postMessage({ type: 'tick' })", "setInterval"];

/**
 * Test-only observer for real heartbeat ticks.
 *
 * WHY AN OBSERVER AT ALL — the per-tick log line the tick handler writes
 * (`diagnostics_manager.rs:418`) is `trace!`, deliberately demoted from
 * `debug!` so it stays off even when console-log collection raises the
 * ceiling to Debug. It is therefore NOT scrapeable from `page.on("console")`
 * at any log level this app runs at. And the tick's downstream effects are
 * invisible in a solo meeting: `maybe_report_stats_to_ui` no-ops because
 * nothing in `dioxus-ui` ever calls `set_stats_callback` (verified by grep — it
 * has no callers anywhere in the repo, only the two definitions at
 * `diagnostics_manager.rs:259` / `:717`), and `send_diagnostic_packets`
 * iterates `fps_trackers`, which is empty with no remote peers. So the tick
 * has to be observed where it crosses into the app: the Worker message.
 *
 * WHAT IT HOOKS — we wrap the global `Worker` in a `Proxy` whose `construct`
 * trap records each instance, and per instance we wrap the `onmessage`
 * *setter*. The handler the wasm client installs (`heartbeat.rs:242-247`,
 * `Closure::wrap(..)` + `worker.set_onmessage(..)`) is decorated with a
 * counter and then forwarded to the native setter, so:
 *
 *   - we count invocations of the PRODUCTION handler — the one that enqueues
 *     `DiagnosticEvent::HeartbeatTick` onto the diagnostics mpsc channel
 *     (`diagnostics_manager.rs:250-254` / `:706-713`) — not the arrival of a
 *     message at a bystander listener the app knows nothing about;
 *   - if the Worker stops posting, the count stops;
 *   - if the client took the fallback `setInterval` backend
 *     (`heartbeat.rs:295`), no Worker exists and the count stays 0.
 *
 * `{ type: 'tick' }` is unique to the heartbeat Worker in this codebase (grep
 * for `'tick'` across `videocall-client`, `videocall-codecs`, `dioxus-ui`,
 * `neteq`: the only hits are `heartbeat.rs`), and the test additionally
 * fetches the ticking Worker's script and asserts it is `HEARTBEAT_WORKER_JS`
 * — so a future Worker that coincidentally posts `{type:'tick'}` cannot mask
 * a dead heartbeat.
 *
 * A `Proxy` is used rather than a `class ... extends Worker` because
 * Playwright serialises this function with `Function.prototype.toString()`
 * AFTER transpiling the spec: a downlevelled `class` can emit references to
 * Babel helper functions (`_inherits`, `_classCallCheck`) that do not exist in
 * the page. `Proxy` + `Reflect.construct` needs no helpers and preserves
 * `instanceof` and the prototype chain — the `construct` trap forwards its
 * `newTarget` argument, so a hypothetical `class X extends Worker` would still
 * receive `X.prototype` rather than `Worker.prototype`. Every other Worker in
 * the app (decoder, neteq) is untouched.
 *
 * The hook is inert if it fails to attach; the `MIN_TICKS_BEFORE_HIDDEN`
 * presence gate is what turns a non-attaching hook into a loud failure.
 */
const HEARTBEAT_OBSERVER_SCRIPT = () => {
  const NativeWorker = window.Worker;
  if (!NativeWorker || window.__vcHeartbeatObserver) {
    return;
  }

  const state: NonNullable<Window["__vcHeartbeatObserver"]> = {
    installed: false,
    workers: [],
  };
  window.__vcHeartbeatObserver = state;

  // `onmessage` is an accessor on `Worker.prototype` (the AbstractWorker IDL
  // event-handler attribute). We need the native setter so the browser still
  // registers the handler for real — shadowing it without forwarding would
  // silently break every Worker in the app, including the decoders.
  const nativeOnMessage = Object.getOwnPropertyDescriptor(NativeWorker.prototype, "onmessage");
  if (!nativeOnMessage || !nativeOnMessage.set || !nativeOnMessage.get) {
    return;
  }
  const nativeSet = nativeOnMessage.set;
  const nativeGet = nativeOnMessage.get;

  window.Worker = new Proxy(NativeWorker, {
    // `newTarget` is forwarded (rather than defaulted to `target`) so that a
    // subclass construction resolves its prototype from the subclass, not from
    // `Worker`. Nothing subclasses `Worker` today; this keeps the wrapper from
    // silently changing semantics if something ever does.
    construct(target, args, newTarget) {
      const worker = Reflect.construct(target, args, newTarget) as Worker;

      const record = { url: String(args[0]), ticks: 0 };
      state.workers.push(record);

      Object.defineProperty(worker, "onmessage", {
        configurable: true,
        get(this: Worker) {
          return nativeGet.call(this);
        },
        set(this: Worker, handler: unknown) {
          // Clearing the handler (or anything non-callable) is passed through
          // untouched so we never change Worker semantics.
          if (typeof handler !== "function") {
            nativeSet.call(this, handler);
            return;
          }
          const wrapped = function (this: Worker, event: MessageEvent) {
            const data = event ? event.data : undefined;
            if (data && data.type === "tick") {
              record.ticks += 1;
            }
            return (handler as (this: Worker, ev: MessageEvent) => unknown).call(this, event);
          };
          nativeSet.call(this, wrapped);
        },
      });

      return worker;
    },
  });
  state.installed = true;
};

/** Snapshot of every Worker the page has built, with its tick count. */
async function readHeartbeatWorkers(page: Page): Promise<Array<{ url: string; ticks: number }>> {
  return page.evaluate(() => {
    const observer = window.__vcHeartbeatObserver;
    if (!observer) {
      return [];
    }
    return observer.workers.map((w) => ({ url: w.url, ticks: w.ticks }));
  });
}

/**
 * Ticks delivered by the single busiest Worker. The client runs TWO heartbeat
 * timers (receive-side `DiagnosticManager` + send-side
 * `SenderDiagnosticManager`, `diagnostics_manager.rs:240` and `:700`), so the
 * total ticks at ~2x cadence. Measuring the max keeps the timing derivation
 * honest: the bound below is satisfiable by ONE worker at
 * `HEARTBEAT_PERIOD_MS`, so it cannot be met by two half-dead ones.
 */
async function readMaxWorkerTicks(page: Page): Promise<number> {
  const workers = await readHeartbeatWorkers(page);
  return workers.reduce((max, w) => Math.max(max, w.ticks), 0);
}

test.describe("Diagnostics heartbeat — tab-visibility throttling fix", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("spawns a worker-backed heartbeat (immune to background-tab throttling)", async ({
    page,
  }) => {
    const meetingId = `e2e_hbworker_${Date.now()}`;

    // Capture client-side info-level logs. We look for the spawn line that
    // `heartbeat.rs::start_worker` writes to confirm we are on the Worker
    // backend rather than the fallback `setInterval`.
    const consoleLines: string[] = [];
    page.on("console", (msg) => {
      // wasm `log::info!` lines come through with type "log" or "info";
      // capture both. We don't filter by message because the heartbeat may
      // produce additional ticks we want to ignore at this stage.
      consoleLines.push(msg.text());
    });

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });
    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("HeartbeatUser", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // The diagnostics managers are constructed up front when the client is
    // created. Give the wasm log a moment to flush its initial messages so
    // the `diagnostics heartbeat: spawned worker` line lands in
    // `consoleLines`. Polling is preferred over a fixed timeout because the
    // initial wasm boot has variable latency.
    await expect
      .poll(
        () => consoleLines.some((line) => line.includes("diagnostics heartbeat: spawned worker")),
        {
          timeout: 15_000,
          message:
            'Expected the heartbeat to spawn a Worker (log line "diagnostics heartbeat: spawned worker"). ' +
            "If you see the fallback line instead, the blob:-Worker construction failed and the fix is not active.",
        },
      )
      .toBeTruthy();

    // Sanity check: we did NOT fall back to the throttled main-thread path.
    const fellBack = consoleLines.some((line) => line.includes("worker unavailable"));
    expect(
      fellBack,
      "heartbeat worker construction unexpectedly failed and fell back to setInterval",
    ).toBe(false);
  });

  /**
   * Issue #720. The previous version of this test was titled "heartbeat keeps
   * ticking after a visibilitychange to hidden" but asserted only that the
   * test's own `document.hidden` override had persisted — it would have passed
   * unchanged if every `HeartbeatTick` stopped the instant the override was
   * applied. This version observes real ticks (see
   * `HEARTBEAT_OBSERVER_SCRIPT`), and the title is narrowed to the claim the
   * assertion actually supports: ticks continue while the page REPORTS hidden.
   * Playwright cannot engage the renderer's real background throttling, so
   * "survives being backgrounded by the OS" stays a manual check.
   */
  test("heartbeat worker keeps delivering ticks while the page reports document.hidden", async ({
    page,
  }) => {
    const meetingId = `e2e_hbhidden_${Date.now()}`;

    // Must be installed BEFORE any page script runs so the wrapped `Worker` is
    // in place by the time the wasm client constructs the heartbeat Worker.
    await page.addInitScript(HEARTBEAT_OBSERVER_SCRIPT);

    await fillAndSubmitJoinForm(page, meetingId, "HeartbeatHidden");

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    // The observer is only useful if it actually attached. Assert that
    // explicitly rather than inferring it from a tick count, so a Playwright /
    // wasm-bindgen change that breaks the `Worker` wrapper reports the real
    // cause instead of looking like a dead heartbeat.
    expect(
      await page.evaluate(() => window.__vcHeartbeatObserver?.installed === true),
      "the heartbeat tick observer failed to install — it must wrap window.Worker before wasm boots",
    ).toBe(true);

    // PRESENCE GATE (pre-condition for the measurement below): ticks are
    // flowing while the tab is plainly visible. Without this a broken observer
    // would leave every counter at 0 and the delta assertion would pass
    // vacuously over a never-populated array.
    await expect
      .poll(() => readMaxWorkerTicks(page), {
        timeout: tickBudgetMs(MIN_TICKS_BEFORE_HIDDEN),
        message:
          "No heartbeat ticks observed while the tab was visible. Either the heartbeat Worker " +
          "never started, or the observer did not intercept its onmessage handler.",
      })
      .toBeGreaterThanOrEqual(MIN_TICKS_BEFORE_HIDDEN);

    // Identify the Worker(s) that produced ticks and prove they are the
    // heartbeat Worker — i.e. that we are counting ticks from the blob: Worker
    // `heartbeat.rs::start_worker` built, not from some other Worker that
    // happens to post a `tick`-shaped message. The blob URL is live until
    // `HeartbeatTimer::drop` revokes it, so it is fetchable from the page.
    const tickingWorkers = (await readHeartbeatWorkers(page)).filter((w) => w.ticks > 0);
    expect(
      tickingWorkers.length,
      "expected at least one Worker to have delivered heartbeat ticks",
    ).toBeGreaterThan(0);

    for (const worker of tickingWorkers) {
      // The fetch is isolated so that a failure (revoked blob URL, a future
      // CSP) surfaces through the guard's assertion message below, with the
      // cause in the received value, instead of rejecting the `evaluate` and
      // killing the test with a bare `TypeError: Failed to fetch`.
      const source = await page.evaluate(async (url) => {
        try {
          const response = await fetch(url);
          return await response.text();
        } catch (err) {
          return `<<worker script unfetchable: ${String(err)}>>`;
        }
      }, worker.url);
      for (const marker of HEARTBEAT_WORKER_JS_MARKERS) {
        expect(
          source,
          `ticking Worker ${worker.url} is not the diagnostics heartbeat Worker ` +
            "(its script does not match heartbeat.rs::HEARTBEAT_WORKER_JS, or could " +
            "not be fetched — see the received value)",
        ).toContain(marker);
      }
    }

    // Confirm the document currently reports visible, so the flip below is a
    // real state change rather than a no-op.
    const initialHidden = await page.evaluate(() => document.hidden);
    expect(initialHidden, "expected the freshly opened tab to be visible").toBe(false);

    const ticksBeforeHidden = await readMaxWorkerTicks(page);

    // Force-hide the document and dispatch the corresponding event. This does
    // NOT trigger Chrome's renderer-level background throttling (see the
    // file-level comment) — it makes every `document.hidden` /
    // `visibilityState` read in the app report hidden and fires the event the
    // app listens for. That is the strongest visibility signal Playwright can
    // produce.
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        get: () => true,
      });
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });

    // THE ASSERTION THIS TEST EXISTS FOR (issue #720): a single heartbeat
    // Worker must deliver MIN_TICKS_AFTER_HIDDEN further ticks to its
    // production `onmessage` consumer while the page reports hidden. If
    // `HeartbeatTick` stops firing when visibility flips, this counter stops
    // advancing and the poll fails — which is exactly what the previous
    // `document.hidden === true` assertion could not detect.
    await expect
      .poll(() => readMaxWorkerTicks(page), {
        timeout: tickBudgetMs(MIN_TICKS_AFTER_HIDDEN),
        message:
          `Heartbeat stalled after the visibilitychange to hidden: expected at least ` +
          `${MIN_TICKS_AFTER_HIDDEN} further ticks from a single Worker at ` +
          `${HEARTBEAT_PERIOD_MS}ms cadence.`,
      })
      .toBeGreaterThanOrEqual(ticksBeforeHidden + MIN_TICKS_AFTER_HIDDEN);

    // The document really did stay hidden for the whole measurement window, so
    // the ticks above were observed under the hidden report and not after some
    // intervening reset of the override.
    expect(
      await page.evaluate(() => document.hidden),
      "document.hidden must still be true — the ticks above must have been observed while hidden",
    ).toBe(true);

    // Restore document visibility for a clean teardown. (Not strictly
    // necessary because the page is about to close, but keeps any
    // follow-up assertions in this file safe to add later.)
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        get: () => false,
      });
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
  });
});
