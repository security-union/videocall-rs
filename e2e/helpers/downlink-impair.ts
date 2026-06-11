/**
 * E2E helper: per-client DOWNLINK impairment for the per-receiver-simulcast
 * divergence test (issue #1080).
 *
 * Lets a spec degrade the downlink of ONE receiver (one BrowserContext) so its
 * simulcast layer chooser steps DOWN while a healthy co-receiver stays high —
 * WITHOUT affecting the sender or any other receiver. This is the infra that
 * un-`fixme`s "congested receiver pulls a LOWER layer than the healthy peer" in
 * `tests/simulcast-per-receiver.spec.ts`.
 *
 * ===========================================================================
 * WHY A THROUGHPUT THROTTLE ACTUALLY PRODUCES LOSS HERE (the verified mechanism)
 * ===========================================================================
 *
 * The chooser steps down on PACKET LOSS / PLI, not on raw bandwidth (see
 * `videocall-client/src/decode/layer_chooser.rs`):
 *   `DownlinkSample::is_congested()` =>
 *       loss_per_sec >= LOSS_STEP_DOWN_PER_SEC (5.0)   // seq gaps off the
 *                                                      // reorder window
 *    || kf_per_sec  >= PLI_STEP_DOWN_PER_SEC  (2.0)    // PLI storm
 *
 * A naive worry is "WS is reliable+ordered TCP, so a bandwidth throttle just
 * DELAYS frames (no loss) and the chooser never steps down." That is true of
 * the browser↔relay TCP segment in isolation — but it ignores the RELAY's
 * per-receiver bounded outbound channel, which is where the loss is actually
 * manufactured:
 *
 *   1. The relay's `WsChatSession` enqueues each outbound frame into a BOUNDED
 *      channel of `WS_OUTBOUND_CHANNEL_CAPACITY = 128` slots
 *      (actix-api/src/constants.rs), drained by a `StreamHandler<Vec<u8>>` that
 *      writes straight to the socket via `ctx.binary(bytes)` — with NO
 *      application-level retransmit (ws_chat_session.rs).
 *
 *   2. Bandwidth-limiting THIS receiver's WS TCP connection makes its kernel
 *      send buffer fill. TCP backpressure stalls `ctx.binary`'s framed sink, so
 *      the drain stops pulling and the 128-slot outbound channel fills.
 *
 *   3. Once full, `outbound_tx.try_send` returns `Full`. The priority-drop
 *      policy (issue #1057) sheds VIDEO/SCREEN frames first (audio protected to
 *      ~95%), increments `videocall_outbound_channel_drops_total`, and fires
 *      `on_outbound_drop` CONGESTION feedback. The shed frame is DISCARDED — it
 *      never reaches this receiver.
 *
 *   4. A discarded frame is a real gap in the receiver's sequence stream. The
 *      receive-side `SequenceTracker` counts it once it shifts off the reorder
 *      window → `loss_per_sec` climbs above 5.0 → the chooser steps this
 *      receiver DOWN. (Freeze-induced PLIs push `kf_per_sec` the same way.)
 *
 *   5. Isolation: each session is its own actor with its own outbound channel.
 *      Only the throttled receiver's channel backs up; the sender and the
 *      healthy receiver are untouched. => deterministic per-receiver divergence.
 *
 * So the loss is RELAY-side overflow, deterministically TRIGGERED by a
 * receiver-side TCP bandwidth limit. A `toxiproxy` `bandwidth` toxic in front of
 * one receiver's WS connection is sufficient; we do not need a loss-injecting
 * netem on the client.
 *
 * ===========================================================================
 * WT (WebTransport / QUIC) PATH — now impairable via the CLIENT-SIDE netsim hook
 * ===========================================================================
 *
 * The toxiproxy mechanism above is RELAY-side overflow triggered by a TCP
 * bandwidth limit, and it only works for WebSocket: toxiproxy is TCP-only and
 * Playwright's `browser.newContext({ proxy })` only carries the browser's
 * TCP/HTTP(S) traffic, so neither can shape the QUIC/UDP datagrams a WebTransport
 * client uses. Per-client UDP `tc qdisc … netem` would need an isolated
 * netns/veth, which the shared-netns Playwright harness does not provide.
 *
 * Issue #1080's WT half solves this WITHOUT any proxy or network namespace by
 * moving the impairment INTO the client. When the dioxus UI is built with the
 * `netsim` cargo feature (the e2e docker image is), every page exposes at boot:
 *
 *   window.__vcNetsim.install(profileName: string, direction: "up"|"down") -> boolean
 *   window.__vcNetsim.clear() -> void
 *
 * `install("crushed_downlink", "down")` installs a per-TAB (thread-local, so
 * per-Playwright-Page) inbound shim that DROPS a fraction of arriving media
 * packets before the receive pipeline sees them. The `crushed_downlink` preset
 * drops ~40% of inbound VIDEO/SCREEN packets — purpose-built so the receiver's
 * `loss_per_sec` crosses the layer chooser's step-down threshold (>= 5 gaps/sec,
 * `LOSS_STEP_DOWN_PER_SEC` in `videocall-client/src/decode/layer_chooser.rs`)
 * within seconds:
 *
 *   dropped inbound media packet  =>  a real gap in that receiver's sequence
 *   stream  =>  the receive-side `SequenceTracker` counts it once it shifts off
 *   the reorder window  =>  `loss_per_sec` climbs above 5.0  =>  the chooser
 *   steps THIS receiver DOWN.
 *
 * Crucial semantics (do NOT overstate them in test comments):
 *   - It is LOSS-ONLY. The shim drops packets; it does NOT emulate bandwidth or
 *     delay on the inbound path.
 *   - It drops ONLY VIDEO/SCREEN media packets. AUDIO and ALL control/heartbeat/
 *     RTT packets always pass, so the impairment cannot flap election or trigger
 *     a reconnection — only the video layer chooser reacts.
 *   - It is PER-TAB. The shim is installed on the degraded receiver's PAGE (the
 *     tab that joined the meeting), NOT on the BrowserContext — so the helper
 *     calls `page.evaluate`, and the sender + healthy receiver (separate tabs/
 *     browsers) are untouched. => deterministic per-receiver divergence.
 *   - It takes effect on the NEXT packet, so mid-call install/clear works: the
 *     climb → impair → heal flow the WT divergence test needs.
 *
 * Because the loss is manufactured client-side, this hook works on BOTH
 * transports and needs NO proxy, NO docker profile, and NO network namespace —
 * it runs against a plain `make e2e-up` stack (provided the UI image carries the
 * `netsim` feature). It is the ONLY per-client option for WebTransport/QUIC.
 *
 * ===========================================================================
 * WHICH MECHANISM TO USE
 * ===========================================================================
 *
 *   - toxiproxy ({@link impairDownlink}) — relay-side outbound-channel overflow,
 *     WebSocket ONLY (TCP). Exercises the relay's #1057 priority-drop shedding
 *     path end-to-end. Requires the `impair` compose profile (toxiproxy):
 *       make e2e-up-impair      # or COMPOSE_PROFILES=impair make e2e-up
 *     Prefer it when the relay-side shedding behaviour is itself under test.
 *
 *   - netsim ({@link impairDownlinkNetsim}) — client-side inbound packet loss,
 *     works on BOTH WebSocket and WebTransport, no proxy/profile needed. The ONLY
 *     per-client option for the WT/QUIC path. Prefer it when the goal is the
 *     RECEIVER's chooser step-down (e.g. the WT per-receiver divergence test) and
 *     a per-client UDP impairment is otherwise impossible.
 *
 * ===========================================================================
 * USAGE
 * ===========================================================================
 *
 * toxiproxy (WS), BEFORE the degraded context navigates:
 *
 *   import { routeDownlinkThroughProxy, impairDownlink, healDownlink }
 *     from "../helpers/downlink-impair";
 *
 *   await routeDownlinkThroughProxy(degradedCtx);   // pins to WS + routes via proxy
 *   // ... join all peers, let layers climb ...
 *   await impairDownlink({ rateKb: 15 });           // ~120 kbps; default if omitted
 *   // ... assert degraded.layerIndex < healthy.layerIndex ...
 *   await healDownlink();
 *
 * netsim (WS or WT), AFTER the degraded receiver's PAGE has joined:
 *
 *   import { impairDownlinkNetsim, healDownlinkNetsim }
 *     from "../helpers/downlink-impair";
 *
 *   // ... join all peers, let layers climb ...
 *   await impairDownlinkNetsim(degradedPage);       // crushed_downlink on the TAB
 *   // ... assert degraded.layerIndex < healthy.layerIndex ...
 *   await healDownlinkNetsim(degradedPage);
 *
 * `routeDownlinkThroughProxy` also pins the context to WebSocket (sets
 * `vc_transport_preference=websocket` in localStorage) because only the WS path
 * is toxiproxy-impairable; without that pin the client could elect WebTransport
 * and bypass the proxy entirely. The netsim hook needs NO such pin — it works on
 * whichever transport the client elects.
 */

import { BrowserContext, Page } from "@playwright/test";

// ---------------------------------------------------------------------------
// Topology constants (must match docker/docker-compose.e2e.yaml `toxiproxy`)
// ---------------------------------------------------------------------------

/** toxiproxy HTTP control API base, published to the host. */
export const TOXIPROXY_API = process.env.TOXIPROXY_API || "http://localhost:8474";

/** Name of the pre-created proxy (see docker/toxiproxy/toxiproxy.json). */
export const WS_PROXY_NAME = "ws-downlink";

/** Shaped WS URL the degraded browser dials instead of `ws://localhost:8080`. */
export const SHAPED_WS_URL = process.env.SHAPED_WS_URL || "ws://localhost:8666";

/** Stable name of the downstream bandwidth toxic we add/remove. */
const TOXIC_NAME = "downlink-bandwidth";

/**
 * Options for {@link impairDownlink}.
 */
export interface ImpairOptions {
  /**
   * Downstream bandwidth cap in KILOBYTES per second applied to the relay→
   * browser direction (toxiproxy `bandwidth` toxic, `rate` field is KB/s).
   *
   * The default 15 KB/s (~120 kbps) is far below a single HD simulcast layer's
   * steady-state byte rate, so the relay's 128-slot outbound channel saturates
   * within a couple of seconds and starts shedding video frames — which is what
   * drives the receiver's `loss_per_sec` over the step-down threshold. Raise it
   * to make the impairment milder; lower it to overflow faster.
   */
  rateKb?: number;
}

// ---------------------------------------------------------------------------
// Browser-side wiring: route ONE context's WS through the proxy
// ---------------------------------------------------------------------------

/**
 * Point a single BrowserContext's media WebSocket at the toxiproxy listener and
 * pin it to the WebSocket transport (the only impairable path today).
 *
 * Implemented as a `GET /config.js` route patch — the SAME technique
 * `enableSimulcastFlag` uses — because `dioxus-ui/scripts/config.js` *reassigns*
 * `window.__APP_CONFIG` wholesale, which would clobber a plain `addInitScript`
 * override. We fetch the real config.js and append an override that rewrites
 * `wsUrl` to {@link SHAPED_WS_URL}. The committed `config.js` is never touched
 * and the patch is scoped to this context only.
 *
 * MUST be called before the context's first navigation.
 *
 * @param context   The degraded receiver's BrowserContext.
 * @param wsUrl     Override target; defaults to {@link SHAPED_WS_URL}.
 */
export async function routeDownlinkThroughProxy(
  context: BrowserContext,
  wsUrl: string = SHAPED_WS_URL,
): Promise<void> {
  // Pin to WebSocket so the client cannot elect WebTransport and bypass the
  // TCP proxy. `vc_transport_preference` is read at boot (context.rs).
  await context.addInitScript((pref) => {
    try {
      window.localStorage.setItem("vc_transport_preference", pref);
      window.localStorage.setItem("vc_transport_preference_sticky", "true");
    } catch {
      /* storage may be unavailable pre-navigation on some origins; ignore */
    }
  }, "websocket");

  // Rewrite wsUrl via the same config.js interception pattern as the simulcast
  // flag helper (full-reassignment-safe).
  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    const original = await response.text();
    const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},{"wsUrl":${JSON.stringify(
      wsUrl,
    )}});`;
    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;
    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: patched,
    });
  });
}

// ---------------------------------------------------------------------------
// toxiproxy control-API calls (enable / adjust / disable the impairment)
// ---------------------------------------------------------------------------

async function toxiproxyFetch(
  path: string,
  init?: { method?: string; body?: unknown },
): Promise<Response> {
  const res = await fetch(`${TOXIPROXY_API}${path}`, {
    method: init?.method ?? "GET",
    headers: init?.body ? { "Content-Type": "application/json" } : undefined,
    body: init?.body ? JSON.stringify(init.body) : undefined,
  });
  return res;
}

/**
 * Throw a clear, actionable error if the toxiproxy control API is unreachable —
 * the usual cause is running without the `impair` compose profile.
 */
export async function assertProxyUp(): Promise<void> {
  let res: Response;
  try {
    res = await toxiproxyFetch(`/proxies/${WS_PROXY_NAME}`);
  } catch (e) {
    throw new Error(
      `toxiproxy control API unreachable at ${TOXIPROXY_API} (${String(e)}). ` +
        "Bring the impairment proxy up with `make e2e-up-impair` " +
        "(or COMPOSE_PROFILES=impair make e2e-up).",
      { cause: e },
    );
  }
  if (!res.ok) {
    throw new Error(
      `toxiproxy proxy '${WS_PROXY_NAME}' not found (HTTP ${res.status}). ` +
        "Is docker/toxiproxy/toxiproxy.json mounted and the `impair` profile up?",
    );
  }
}

/**
 * Apply (or update) a downstream bandwidth cap on the degraded receiver's WS
 * connection. Idempotent: re-applying updates the existing toxic's rate rather
 * than erroring on a duplicate name.
 *
 * `stream: "downstream"` shapes the relay→browser direction (the receiver's
 * DOWNLINK), which is exactly the direction the layer chooser reacts to.
 */
export async function impairDownlink(opts: ImpairOptions = {}): Promise<void> {
  await assertProxyUp();
  const rateKb = opts.rateKb ?? 15; // ~120 kbps; see ImpairOptions.rateKb

  // Try to create; if it already exists (409), update it instead.
  const create = await toxiproxyFetch(`/proxies/${WS_PROXY_NAME}/toxics`, {
    method: "POST",
    body: {
      name: TOXIC_NAME,
      type: "bandwidth",
      stream: "downstream",
      toxicity: 1.0,
      attributes: { rate: rateKb },
    },
  });
  if (create.ok) return;

  if (create.status === 409) {
    const update = await toxiproxyFetch(`/proxies/${WS_PROXY_NAME}/toxics/${TOXIC_NAME}`, {
      method: "POST",
      body: { attributes: { rate: rateKb } },
    });
    if (!update.ok) {
      throw new Error(
        `Failed to update existing '${TOXIC_NAME}' toxic (HTTP ${update.status}): ${await update.text()}`,
      );
    }
    return;
  }

  throw new Error(
    `Failed to add '${TOXIC_NAME}' bandwidth toxic (HTTP ${create.status}): ${await create.text()}`,
  );
}

/**
 * Remove the bandwidth toxic so the degraded receiver's downlink recovers
 * (used to assert climb-back, or in test teardown). Tolerates a missing toxic.
 */
export async function healDownlink(): Promise<void> {
  let res: Response;
  try {
    res = await toxiproxyFetch(`/proxies/${WS_PROXY_NAME}/toxics/${TOXIC_NAME}`, {
      method: "DELETE",
    });
  } catch {
    // Proxy not up at all — nothing to heal.
    return;
  }
  // 404 == already gone; treat as success.
  if (!res.ok && res.status !== 404) {
    throw new Error(
      `Failed to remove '${TOXIC_NAME}' toxic (HTTP ${res.status}): ${await res.text()}`,
    );
  }
}

// ---------------------------------------------------------------------------
// netsim: client-side per-TAB inbound packet loss (WS + WT). See the header
// "WT path" section for the full mechanism and semantics.
// ---------------------------------------------------------------------------

/**
 * The default netsim downlink profile. `crushed_downlink` drops ~40% of inbound
 * VIDEO/SCREEN packets (AUDIO + control/RTT always pass) so the receiver's
 * `loss_per_sec` crosses the layer chooser's >= 5 gaps/sec step-down threshold
 * within seconds. Defined Rust-side under the `netsim` cargo feature.
 */
export const DEFAULT_NETSIM_DOWNLINK_PROFILE = "crushed_downlink";

/** Shape of the `window.__vcNetsim` hook the `netsim`-built UI exposes per tab. */
interface VcNetsim {
  install(profile: string, direction: "up" | "down"): boolean;
  clear(): void;
}

declare global {
  interface Window {
    __vcNetsim?: VcNetsim;
  }
}

/**
 * Throw a clear, actionable error if `window.__vcNetsim` is absent on `page` —
 * which means the dioxus UI image was built WITHOUT the `netsim` cargo feature.
 * Call this before relying on the hook so a feature-less image fails loud with a
 * rebuild instruction rather than a confusing `undefined.install` TypeError.
 *
 * MUST be called on the receiver's PAGE (the tab that joined), after navigation
 * so the boot-time hook install has run.
 */
export async function assertNetsimAvailable(page: Page): Promise<void> {
  const present = await page.evaluate(() => typeof window.__vcNetsim?.install === "function");
  if (!present) {
    throw new Error(
      "window.__vcNetsim is not available on this page — the dioxus UI image was built " +
        "WITHOUT the `netsim` cargo feature, so the client-side downlink-impairment hook " +
        "does not exist. Rebuild the e2e UI image with the netsim feature wired in " +
        "(`make e2e-build`) and re-run. (This hook is the per-client impairment the WT " +
        "per-receiver divergence test depends on; see helpers/downlink-impair.ts header.)",
    );
  }
}

/**
 * Install the client-side inbound packet-loss shim on a SINGLE receiver's PAGE
 * (per-tab), so ONLY that receiver's downlink is degraded. Drops VIDEO/SCREEN
 * media packets only; AUDIO and control/RTT always pass (see header). Takes
 * effect on the next packet, so it is safe to call mid-call.
 *
 * @param page    The degraded receiver's Page (the tab that joined the meeting).
 *                MUST be this page, NOT a BrowserContext — the shim is per-tab.
 * @param profile The netsim profile to install; defaults to
 *                {@link DEFAULT_NETSIM_DOWNLINK_PROFILE} (`crushed_downlink`).
 */
export async function impairDownlinkNetsim(
  page: Page,
  profile: string = DEFAULT_NETSIM_DOWNLINK_PROFILE,
): Promise<void> {
  await assertNetsimAvailable(page);
  const installed = await page.evaluate((p) => window.__vcNetsim!.install(p, "down"), profile);
  if (!installed) {
    throw new Error(
      `window.__vcNetsim.install("${profile}", "down") returned false — the UI rejected ` +
        "the profile/direction (unknown netsim profile name?). Confirm the Rust `netsim` " +
        `feature defines a "${profile}" downlink preset.`,
    );
  }
}

/**
 * Clear the netsim impairment on a receiver's PAGE so its downlink recovers
 * (used to assert climb-back, or in teardown). Clears BOTH directions, which is
 * what we want for a receiver that only ever installed the downlink shim. No-op
 * if the hook is absent (e.g. the page was closed or the image lacks `netsim`),
 * so it is safe to call unconditionally in a `finally`.
 */
export async function healDownlinkNetsim(page: Page): Promise<void> {
  // Tolerate a closed page / missing hook: this is teardown-safe by design.
  await page
    .evaluate(() => {
      if (typeof window.__vcNetsim?.clear === "function") {
        window.__vcNetsim.clear();
      }
    })
    .catch(() => {
      /* page closed or hook absent — nothing to heal */
    });
}
